//! One claimed row: read the article, last lease check, call, release
//! (ADR 0011 §7, §10). Runs inside the window's `JoinSet`; never writes
//! events (the coordinator does, after the window).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use rss_ai_news_decision::{
    DecisionError, DecisionRequest, Exchange, RawInfo, RawResponsePolicy, answers_document,
};
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_storage::{
    ClaimedDecision, DecisionAttemptRecord, DecisionFailure, DecisionFailureMode,
    DecisionRawRecord, DecisionResultState, DecisionSuccess, StorageError,
};
use time::OffsetDateTime;

use super::{CategoryPlan, DecisionRunDeps};

/// §10 classes. `Global*` stop the run; `Item*` concern one article.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    ItemRetryable,
    ItemPermanent,
    /// Not about the article: refund the attempt.
    GlobalUnrelated,
    /// Probably configuration: the trigger row is charged.
    GlobalSuspect,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ItemRetryable => "item_retryable",
            Self::ItemPermanent => "item_permanent",
            Self::GlobalUnrelated => "global_unrelated",
            Self::GlobalSuspect => "global_suspect",
        }
    }
}

pub fn classify(error: &DecisionError) -> FailureClass {
    use DecisionError as E;
    match error {
        E::Timeout { .. } | E::RequestTimeout | E::Unavailable { .. } | E::Server { .. } => {
            FailureClass::ItemRetryable
        }
        E::Transport => FailureClass::ItemRetryable,
        E::InvalidResponse { .. } | E::PayloadTooLarge | E::ResponseTooLarge { .. } => {
            FailureClass::ItemPermanent
        }
        E::Rejected { code, .. } if code.as_deref() == Some("max_tokens_exceeded") => {
            FailureClass::ItemPermanent
        }
        E::Unauthorized { .. }
        | E::Connection
        | E::RateLimited { .. }
        | E::InvalidConfig(_)
        | E::InvalidRequest(_) => FailureClass::GlobalUnrelated,
        E::Rejected { .. } | E::UnsupportedEncoding | E::HttpStatus { .. } => {
            FailureClass::GlobalSuspect
        }
    }
}

/// Why the whole run stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Abort {
    /// A provider error of a global class.
    Provider {
        class: FailureClass,
        kind: String,
        config: bool,
        rate_limited: bool,
    },
    /// Storage error, storage timeout, owner mismatch, panic, drain timeout
    /// or a defensive identity mismatch.
    Infrastructure { kind: &'static str },
}

impl Abort {
    pub fn kind(&self) -> &str {
        match self {
            Self::Provider { kind, .. } => kind,
            Self::Infrastructure { kind } => kind,
        }
    }
}

/// What happened to one row, for the summary and the events.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub category_index: usize,
    pub row_id: i64,
    pub status: TaskStatus,
    pub input_tokens: u64,
    pub raw_withheld: Option<&'static str>,
    pub retry_after_seconds: Option<u64>,
    /// The category stops for this run.
    pub pause: bool,
    /// The provider answered 429 (kept even when a later fault decides the
    /// stop).
    pub rate_limited: bool,
    /// A storage fault seen before the release (`storage_error`), counted
    /// apart from the row's final status.
    pub fault: Option<&'static str>,
    pub abort: Option<Abort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Succeeded,
    /// Released with this state after a provider error.
    Failed {
        class: FailureClass,
        kind: String,
        state: DecisionResultState,
    },
    Refunded {
        reason: &'static str,
    },
    /// The row's state is not known (release failed, timed out or missed
    /// the owner guard); lease reclaim takes care of it.
    Unknown {
        kind: &'static str,
    },
}

pub(super) struct TaskInput {
    pub deps: Arc<DecisionRunDeps>,
    pub plan: Arc<CategoryPlan>,
    pub category_index: usize,
    pub row: ClaimedDecision,
    pub owner: Arc<str>,
    /// `lease_expires_at − 5 s` on the monotonic clock, measured from before
    /// the claim statement (the stored expiry was computed then).
    pub row_deadline: Instant,
    /// Client calls of the run, bumped just before each call so that panics
    /// and cancellations are still counted against the budget.
    pub calls: Arc<AtomicU64>,
}

fn storage_deadline() -> Duration {
    Duration::from_secs(u64::from(rss_ai_news_decision::STORAGE_DEADLINE_SECONDS))
}

/// A storage call bounded by `D` and the row deadline.
async fn bounded<T>(
    row_deadline: Instant,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    let limit = storage_deadline().min(row_deadline.saturating_duration_since(Instant::now()));
    tokio::time::timeout(limit, future).await.ok()
}

impl TaskOutcome {
    fn new(input: &TaskInput, status: TaskStatus) -> Self {
        Self {
            category_index: input.category_index,
            row_id: input.row.id,
            rate_limited: false,
            fault: None,
            status,
            input_tokens: 0,
            raw_withheld: None,
            retry_after_seconds: None,
            pause: false,
            abort: None,
        }
    }
}

pub(super) async fn process(input: TaskInput) -> TaskOutcome {
    let deps = Arc::clone(&input.deps);
    let plan = Arc::clone(&input.plan);
    let row = &input.row;

    if row.policy_version != plan.identity.policy_version
        || row.execution_key != plan.identity.execution_key
    {
        return refund(&input, "identity_mismatch", true).await;
    }

    let article = match bounded(
        input.row_deadline,
        deps.decision_repo.load_article(row.article_id),
    )
    .await
    {
        Some(Ok(Some(article))) => article,
        Some(Ok(None)) => {
            let failure = DecisionFailure {
                mode: DecisionFailureMode::Permanent,
                kind: "article_missing".to_string(),
                message: "article row not found".to_string(),
                max_attempts: deps.settings.max_attempts,
                attempt: DecisionAttemptRecord::default(),
            };
            return release_failure(
                &input,
                failure,
                FailureClass::ItemPermanent,
                TaskOutcome::new(
                    &input,
                    TaskStatus::Refunded {
                        reason: "article_missing",
                    },
                ),
            )
            .await;
        }
        // A timeout raised by the server or the pool is as unknown as one
        // seen here.
        Some(Err(StorageError::Timeout)) => {
            return unknown(
                TaskOutcome::new(&input, TaskStatus::Refunded { reason: "" }),
                "storage_timeout",
            );
        }
        // A failed read changed nothing, so the row can be refunded.
        Some(Err(_)) => {
            let mut outcome = refund(&input, "storage_error", true).await;
            outcome.fault = Some("storage_error");
            return outcome;
        }
        // A timed-out storage call has an unknown result: leave the row to
        // lease reclaim rather than issue another write (ADR 0011 §7).
        None => {
            return unknown(
                TaskOutcome::new(&input, TaskStatus::Refunded { reason: "" }),
                "storage_timeout",
            );
        }
    };

    let call_budget = deps.settings.request_timeout + storage_deadline();
    if input.row_deadline.saturating_duration_since(Instant::now()) < call_budget {
        let mut outcome = refund(&input, "lease_insufficient", false).await;
        outcome.pause = true;
        return outcome;
    }

    let call_deadline = (Instant::now() + deps.settings.request_timeout)
        .min(input.row_deadline - storage_deadline());
    input.calls.fetch_add(1, Ordering::SeqCst);
    let started = Instant::now();
    let exchange = deps
        .client
        .decide(
            &DecisionRequest {
                title: &article.title,
                body: &article.body_text,
                questions: &plan.questions,
            },
            call_deadline,
        )
        .await;
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    finish(&input, exchange, latency_ms).await
}

fn attempt_record(
    deps: &DecisionRunDeps,
    exchange: &Exchange,
    latency_ms: i64,
    keep_body: bool,
    now: OffsetDateTime,
) -> DecisionAttemptRecord {
    let sent = exchange.sent.as_ref();
    DecisionAttemptRecord {
        endpoint_origin: Some(deps.endpoint_origin.clone()),
        state_sha256: sent.map(|sent| sent.state_sha256.clone()),
        body_chars_sent: sent.map(|sent| i64::from(sent.body_chars)),
        body_truncated: sent.map(|sent| sent.body_truncated),
        provider_truncated: None,
        state_tokens_dropped: None,
        input_tokens: None,
        latency_ms: Some(latency_ms),
        raw: exchange
            .raw
            .as_ref()
            .map(|raw| raw_record(deps, raw, keep_body, now)),
    }
}

fn raw_record(
    deps: &DecisionRunDeps,
    raw: &RawInfo,
    keep_body: bool,
    now: OffsetDateTime,
) -> DecisionRawRecord {
    let ttl_days = deps.settings.raw_response_ttl_days;
    let body = raw
        .body
        .as_ref()
        .filter(|_| keep_body)
        // Capturable bodies are JSON, hence UTF-8.
        .and_then(|body| String::from_utf8(body.clone()).ok());
    DecisionRawRecord {
        expires_at: (body.is_some() && ttl_days > 0)
            .then(|| now + time::Duration::days(i64::from(ttl_days))),
        body,
        bytes: i64::try_from(raw.bytes).unwrap_or(i64::MAX),
        sha256: raw.sha256.clone(),
        withheld: raw.withheld.map(|withheld| withheld.as_str()),
    }
}

async fn finish(input: &TaskInput, exchange: Exchange, latency_ms: i64) -> TaskOutcome {
    let deps = &input.deps;
    let now = OffsetDateTime::now_utc();
    let raw_withheld = exchange
        .raw
        .as_ref()
        .and_then(|raw| raw.withheld)
        .map(|withheld| withheld.as_str());
    let policy = deps.settings.raw_response;
    match &exchange.result {
        Ok(response) => {
            let keep_body = policy == RawResponsePolicy::Always;
            let mut attempt = attempt_record(deps, &exchange, latency_ms, keep_body, now);
            attempt.input_tokens =
                Some(i64::try_from(response.usage.input_tokens).unwrap_or(i64::MAX));
            if let rss_ai_news_decision::StateTruncation::Laya {
                truncated,
                state_tokens_dropped,
            } = response.usage.state
            {
                attempt.provider_truncated = Some(truncated);
                attempt.state_tokens_dropped =
                    Some(i64::try_from(state_tokens_dropped).unwrap_or(i64::MAX));
            }
            let Some(document) = answers_document(&input.plan.questions, &response.answers) else {
                // Unreachable for a validated response; never store a guess.
                let failure = DecisionFailure {
                    mode: DecisionFailureMode::Permanent,
                    kind: "decision_invalid_response".to_string(),
                    message: "answers do not line up with the questions".to_string(),
                    max_attempts: deps.settings.max_attempts,
                    attempt,
                };
                let mut base = TaskOutcome::new(input, TaskStatus::Refunded { reason: "" });
                base.raw_withheld = raw_withheld;
                return release_failure(input, failure, FailureClass::ItemPermanent, base).await;
            };
            let success = DecisionSuccess {
                answers_json: document.to_string(),
                reported_model: response.execution.reported_model.clone(),
                routing_model: response.execution.routing_model.clone(),
                attempt,
            };
            let mut outcome = TaskOutcome::new(input, TaskStatus::Succeeded);
            outcome.raw_withheld = raw_withheld;
            outcome.input_tokens = response.usage.input_tokens;
            match bounded(
                input.row_deadline,
                deps.decision_repo
                    .release_success(input.row.id, &input.owner, &success, now),
            )
            .await
            {
                Some(Ok(true)) => outcome,
                Some(Ok(false)) => unknown(outcome, "owner_mismatch"),
                Some(Err(StorageError::Timeout)) | None => unknown(outcome, "storage_timeout"),
                Some(Err(_)) => unknown(outcome, "release_failed"),
            }
        }
        Err(error) => {
            let class = classify(error);
            let mode = match class {
                FailureClass::ItemRetryable | FailureClass::GlobalSuspect => {
                    DecisionFailureMode::Charge
                }
                FailureClass::ItemPermanent => DecisionFailureMode::Permanent,
                FailureClass::GlobalUnrelated => DecisionFailureMode::Refund,
            };
            let keep_body = policy != RawResponsePolicy::Never;
            let failure = DecisionFailure {
                mode,
                kind: error.error_kind().to_string(),
                message: error.display_user(),
                max_attempts: deps.settings.max_attempts,
                attempt: attempt_record(deps, &exchange, latency_ms, keep_body, now),
            };
            let mut base = TaskOutcome::new(input, TaskStatus::Refunded { reason: "" });
            base.raw_withheld = raw_withheld;
            if let DecisionError::RateLimited {
                retry_after_seconds,
            } = error
            {
                base.retry_after_seconds = *retry_after_seconds;
                base.rate_limited = true;
            }
            match class {
                FailureClass::ItemRetryable => base.pause = true,
                FailureClass::ItemPermanent => {}
                FailureClass::GlobalUnrelated | FailureClass::GlobalSuspect => {
                    base.abort = Some(Abort::Provider {
                        class,
                        kind: error.error_kind().to_string(),
                        config: matches!(
                            error,
                            DecisionError::Unauthorized { .. }
                                | DecisionError::InvalidConfig(_)
                                | DecisionError::InvalidRequest(_)
                        ),
                        rate_limited: matches!(error, DecisionError::RateLimited { .. }),
                    });
                }
            }
            release_failure(input, failure, class, base).await
        }
    }
}

/// The row's state is unknown: an infrastructure stop that outranks a rate
/// limit (exit 1 over 0) but not a configuration stop (78).
fn unknown(mut outcome: TaskOutcome, kind: &'static str) -> TaskOutcome {
    outcome.status = TaskStatus::Unknown { kind };
    if !matches!(outcome.abort, Some(Abort::Provider { config: true, .. })) {
        outcome.abort = Some(Abort::Infrastructure { kind });
    }
    outcome
}

async fn release_failure(
    input: &TaskInput,
    failure: DecisionFailure,
    class: FailureClass,
    mut outcome: TaskOutcome,
) -> TaskOutcome {
    let deps = &input.deps;
    let kind = failure.kind.clone();
    let refunded = failure.mode == DecisionFailureMode::Refund;
    match bounded(
        input.row_deadline,
        deps.decision_repo.release_failure(
            input.row.id,
            &input.owner,
            &failure,
            OffsetDateTime::now_utc(),
        ),
    )
    .await
    {
        Some(Ok(Some(state))) => {
            outcome.status = if refunded {
                TaskStatus::Refunded {
                    reason: "provider_unrelated",
                }
            } else {
                TaskStatus::Failed { class, kind, state }
            };
            outcome
        }
        Some(Ok(None)) => unknown(outcome, "owner_mismatch"),
        Some(Err(StorageError::Timeout)) | None => unknown(outcome, "storage_timeout"),
        Some(Err(_)) => unknown(outcome, "release_failed"),
    }
}

/// Releases the row back to `pending` with the attempt refunded, without
/// calling the provider. `abort` marks an infrastructure stop.
async fn refund(input: &TaskInput, reason: &'static str, abort: bool) -> TaskOutcome {
    let failure = DecisionFailure {
        mode: DecisionFailureMode::Refund,
        kind: format!("decision_run_{reason}"),
        message: format!("not sent: {reason}"),
        max_attempts: input.deps.settings.max_attempts,
        attempt: DecisionAttemptRecord::default(),
    };
    let mut outcome = TaskOutcome::new(input, TaskStatus::Refunded { reason });
    if abort {
        outcome.abort = Some(Abort::Infrastructure { kind: reason });
    }
    let released = release_failure(input, failure, FailureClass::GlobalUnrelated, outcome).await;
    match released.status {
        TaskStatus::Refunded { .. } => TaskOutcome {
            status: TaskStatus::Refunded { reason },
            ..released
        },
        _ => released,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_class() {
        use DecisionError as E;
        let cases = [
            (E::Timeout { seconds: 1 }, FailureClass::ItemRetryable),
            (E::RequestTimeout, FailureClass::ItemRetryable),
            (
                E::Unavailable {
                    status: 503,
                    code: None,
                },
                FailureClass::ItemRetryable,
            ),
            (E::Server { status: 500 }, FailureClass::ItemRetryable),
            (E::Transport, FailureClass::ItemRetryable),
            (
                E::InvalidResponse { reason: "x".into() },
                FailureClass::ItemPermanent,
            ),
            (E::PayloadTooLarge, FailureClass::ItemPermanent),
            (
                E::ResponseTooLarge { limit: 1 },
                FailureClass::ItemPermanent,
            ),
            (
                E::Rejected {
                    status: 400,
                    code: Some("max_tokens_exceeded".into()),
                },
                FailureClass::ItemPermanent,
            ),
            (
                E::Unauthorized { status: 401 },
                FailureClass::GlobalUnrelated,
            ),
            (E::Connection, FailureClass::GlobalUnrelated),
            (
                E::RateLimited {
                    retry_after_seconds: None,
                },
                FailureClass::GlobalUnrelated,
            ),
            (E::InvalidConfig("x".into()), FailureClass::GlobalUnrelated),
            (E::InvalidRequest("x".into()), FailureClass::GlobalUnrelated),
            (
                E::Rejected {
                    status: 422,
                    code: Some("VALIDATION_ERROR".into()),
                },
                FailureClass::GlobalSuspect,
            ),
            (E::UnsupportedEncoding, FailureClass::GlobalSuspect),
            (E::HttpStatus { status: 404 }, FailureClass::GlobalSuspect),
        ];
        for (error, class) in cases {
            assert_eq!(classify(&error), class, "{error:?}");
        }
    }
}
