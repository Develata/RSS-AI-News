//! decision-run: the typed-decision shadow runner (ADR 0011). Reads articles,
//! asks one provider fixed-type questions and records the answers in
//! `decision_results`. Writes nothing else: no `articles.state`, no AI result,
//! no artifact.
//!
//! Per run: raw cleanup → policy snapshots → per-identity maintenance → rounds
//! over the categories, each step claiming one window of at most
//! `concurrency` rows that are all dispatched at once and fully joined before
//! the next step. Correctness rests on the unique key, the owner guard and the
//! call budget, not on timing (ADR 0011 §7).

mod task;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use rss_ai_news_decision::{DecisionClient, QuestionSpec, RawResponsePolicy};
use rss_ai_news_storage::{
    ClaimedDecision, DecisionExecution, DecisionIdentity, DecisionResultRepository,
    RunEventRepository, build_owner_id,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio::task::JoinSet;

use crate::{context::RunMeta, events::RunEventEmitter};

pub use task::{Abort, FailureClass, TaskOutcome, TaskStatus, classify};

const STAGE: &str = "decision_run";
const RAW_PURGE_BATCH: u32 = 500;
/// After `abort_all`, how long the coordinator still collects results.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct DecisionRunSettings {
    pub concurrency: u32,
    pub lease_seconds: u32,
    pub max_attempts: u32,
    pub request_timeout: Duration,
    pub raw_response: RawResponsePolicy,
    pub raw_response_ttl_days: u32,
}

pub struct DecisionRunDeps {
    pub run: RunMeta,
    pub settings: DecisionRunSettings,
    pub client: Arc<dyn DecisionClient>,
    /// `scheme://host[:port]` of the provider, recorded with each result.
    pub endpoint_origin: String,
    pub execution: DecisionExecution,
    pub execution_key: String,
    /// Also reads the articles, so every statement is bounded (ADR 0011 §7).
    pub decision_repo: Arc<dyn DecisionResultRepository>,
    /// Production passes the same `DecisionResultRepo`, whose inserts are
    /// bounded on PostgreSQL.
    pub event_repo: Arc<dyn RunEventRepository>,
}

/// One category's current identity and questions.
#[derive(Debug, Clone)]
pub struct CategoryPlan {
    pub identity: DecisionIdentity,
    pub questions: Vec<QuestionSpec>,
    pub canonical_policy_json: String,
}

#[derive(Debug, Clone)]
pub struct DecisionRunOptions {
    /// In configuration order; every plan has questions.
    pub categories: Vec<CategoryPlan>,
    pub max_requests: u32,
    pub window_hours: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CategoryStats {
    pub category: String,
    pub generated: u64,
    pub claimed: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub retried: u64,
    pub refunded: u64,
    pub other_identity_pending: u64,
    /// Why the category stopped early in this run.
    pub paused: Option<&'static str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionRunSummary {
    pub categories: Vec<CategoryStats>,
    pub claimed: u64,
    /// Client calls, i.e. spent request budget.
    pub calls: u64,
    pub input_tokens: u64,
    pub budget_exhausted: bool,
    pub rate_limited: bool,
    pub aborted: Option<Abort>,
    pub storage_errors: u64,
    pub storage_timeouts: u64,
    pub release_failed: u64,
    pub owner_mismatch: u64,
    pub tasks_panicked: u64,
    pub drain_timeout: bool,
    pub raw_purged: u64,
    pub raw_gc_backlog: bool,
}

impl DecisionRunSummary {
    /// 78 for configuration / credential stops, 1 for other run-level stops,
    /// 0 otherwise (single-article failures never change it).
    pub fn exit_class(&self) -> u8 {
        match &self.aborted {
            Some(Abort::Provider { config: true, .. }) => 78,
            Some(Abort::Provider {
                rate_limited: true, ..
            }) => 0,
            Some(_) => 1,
            None if self.drain_timeout => 1,
            None => 0,
        }
    }

    fn abort(&mut self, abort: Abort) {
        // Keep the most severe stop: configuration first, then anything else.
        let severity = |abort: &Abort| match abort {
            Abort::Provider { config: true, .. } => 2,
            Abort::Provider {
                rate_limited: true, ..
            } => 0,
            _ => 1,
        };
        if self
            .aborted
            .as_ref()
            .is_none_or(|current| severity(&abort) > severity(current))
        {
            self.aborted = Some(abort);
        }
    }
}

pub struct DecisionRunFlow {
    deps: Arc<DecisionRunDeps>,
    /// Client calls so far; the request budget is settled against it.
    calls: Arc<AtomicU64>,
}

fn storage_deadline() -> Duration {
    Duration::from_secs(u64::from(rss_ai_news_decision::STORAGE_DEADLINE_SECONDS))
}

impl DecisionRunFlow {
    pub fn new(deps: Arc<DecisionRunDeps>) -> Self {
        Self {
            deps,
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    fn emitter(&self) -> RunEventEmitter<'_> {
        RunEventEmitter {
            run_id: &self.deps.run.run_id,
            stage: STAGE,
            repo: self.deps.event_repo.as_ref(),
        }
    }

    /// An event bounded by D; a failure is only logged (the emitter does).
    async fn emit(
        &self,
        kind: &str,
        severity: &str,
        target_id: Option<i64>,
        message: &str,
        context: serde_json::Value,
    ) {
        let emitter = self.emitter();
        let target = target_id.map(|_| "decision_result");
        if tokio::time::timeout(
            storage_deadline(),
            emitter.emit(kind, severity, target, target_id, message, Some(context)),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                stage = STAGE,
                event_kind = kind,
                "run_event write timed out"
            );
        }
    }

    /// A storage call bounded by D; `Err(None)` is a timeout.
    async fn storage<T, E: std::fmt::Display>(
        future: impl std::future::Future<Output = Result<T, E>>,
    ) -> Result<T, Option<String>> {
        match tokio::time::timeout(storage_deadline(), future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(Some(error.to_string())),
            Err(_) => Err(None),
        }
    }

    fn storage_failure(summary: &mut DecisionRunSummary, error: Option<String>, step: &str) {
        match error {
            Some(error) => {
                tracing::error!(stage = STAGE, step, "storage error: {error}");
                summary.storage_errors += 1;
                summary.abort(Abort::Infrastructure {
                    kind: "storage_error",
                });
            }
            None => {
                tracing::error!(stage = STAGE, step, "storage call timed out");
                summary.storage_timeouts += 1;
                summary.abort(Abort::Infrastructure {
                    kind: "storage_timeout",
                });
            }
        }
    }

    pub async fn run(&self, opts: &DecisionRunOptions) -> DecisionRunSummary {
        let deps = &self.deps;
        let mut summary = DecisionRunSummary {
            categories: opts
                .categories
                .iter()
                .map(|plan| CategoryStats {
                    category: plan.identity.category_key.clone(),
                    ..CategoryStats::default()
                })
                .collect(),
            ..DecisionRunSummary::default()
        };
        self.emit(
            "run_started",
            "info",
            None,
            "decision run started",
            json!({
                "categories": summary.categories.iter().map(|c| &c.category).collect::<Vec<_>>(),
                "execution_key": deps.execution_key,
                "max_requests": opts.max_requests,
            }),
        )
        .await;

        // Frozen once (ADR 0011 §5).
        let started = OffsetDateTime::now_utc();
        let cutoff = started - time::Duration::hours(i64::from(opts.window_hours));

        self.purge_raw(opts, &mut summary).await;
        if summary.aborted.is_none() {
            self.prepare(opts, &mut summary).await;
        }
        if summary.aborted.is_none() {
            self.rounds(opts, cutoff, &mut summary).await;
        }

        let aborted = summary
            .aborted
            .as_ref()
            .map(|abort| abort.kind().to_string());
        if let Some(kind) = &aborted {
            self.emit(
                "decision_run_aborted",
                "warn",
                None,
                "decision run stopped early",
                json!({ "kind": kind }),
            )
            .await;
        }
        self.emit(
            "run_completed",
            "info",
            None,
            "decision run completed",
            json!({
                "claimed": summary.claimed,
                "calls": summary.calls,
                "input_tokens": summary.input_tokens,
                "budget_exhausted": summary.budget_exhausted,
                "rate_limited": summary.rate_limited,
                "aborted": aborted,
                "drain_timeout": summary.drain_timeout,
            }),
        )
        .await;
        summary
    }

    /// Raw cleanup across all identities, at most `max(1000, 2 × budget)`
    /// rows per run (ADR 0011 §8). `raw_gc_backlog`: the cap was reached, so
    /// expired bodies may remain.
    async fn purge_raw(&self, opts: &DecisionRunOptions, summary: &mut DecisionRunSummary) {
        let cap = u64::from(opts.max_requests.saturating_mul(2).max(1000));
        let now = OffsetDateTime::now_utc();
        loop {
            let remaining = cap - summary.raw_purged;
            if remaining == 0 {
                summary.raw_gc_backlog = true;
                break;
            }
            let batch = u32::try_from(remaining.min(u64::from(RAW_PURGE_BATCH))).unwrap_or(0);
            match Self::storage(self.deps.decision_repo.purge_expired_raw(now, batch)).await {
                Ok(count) => {
                    summary.raw_purged += count;
                    if count < u64::from(batch) {
                        break;
                    }
                }
                Err(error) => {
                    Self::storage_failure(summary, error, "purge_expired_raw");
                    return;
                }
            }
        }
        if summary.raw_purged > 0 {
            self.emit(
                "raw_responses_expired",
                "info",
                None,
                "expired raw decision responses cleared",
                json!({ "count": summary.raw_purged, "backlog": summary.raw_gc_backlog }),
            )
            .await;
        }
    }

    /// Policy snapshots, then per-identity reclaim and budget sweep.
    async fn prepare(&self, opts: &DecisionRunOptions, summary: &mut DecisionRunSummary) {
        let repo = &self.deps.decision_repo;
        let max_attempts = self.deps.settings.max_attempts;
        for (index, plan) in opts.categories.iter().enumerate() {
            let now = OffsetDateTime::now_utc();
            let identity = &plan.identity;
            if let Err(error) = Self::storage(repo.upsert_policy(
                &identity.policy_version,
                &plan.canonical_policy_json,
                now,
            ))
            .await
            {
                return Self::storage_failure(summary, error, "upsert_policy");
            }
            let reclaimed = match Self::storage(repo.reclaim_expired(identity, now)).await {
                Ok(count) => count,
                Err(error) => return Self::storage_failure(summary, error, "reclaim_expired"),
            };
            let swept = match Self::storage(repo.terminalize_exhausted(identity, max_attempts, now))
                .await
            {
                Ok(count) => count,
                Err(error) => {
                    return Self::storage_failure(summary, error, "terminalize_exhausted");
                }
            };
            if reclaimed > 0 {
                self.emit(
                    "leases_reclaimed",
                    "info",
                    None,
                    "expired decision leases reclaimed at run start",
                    json!({ "category": identity.category_key, "count": reclaimed }),
                )
                .await;
            }
            if swept > 0 {
                self.emit(
                    "retry_budget_swept",
                    "warn",
                    None,
                    "decision rows with a spent attempt budget failed",
                    json!({ "category": identity.category_key, "count": swept }),
                )
                .await;
            }
            // Informational only: a failure here does not stop the run.
            match Self::storage(repo.count_other_identity_pending(identity)).await {
                Ok(count) => summary.categories[index].other_identity_pending = count,
                Err(_) => tracing::warn!(
                    stage = STAGE,
                    category = %identity.category_key,
                    "could not count other-identity pending rows"
                ),
            }
        }
    }

    async fn rounds(
        &self,
        opts: &DecisionRunOptions,
        cutoff: OffsetDateTime,
        summary: &mut DecisionRunSummary,
    ) {
        let deps = &self.deps;
        let plans: Vec<Arc<CategoryPlan>> = opts.categories.iter().cloned().map(Arc::new).collect();
        let owner: Arc<str> = Arc::from(build_owner_id());
        let mut budget = opts.max_requests;
        let mut idle = vec![false; plans.len()];
        loop {
            let mut progressed = false;
            for (index, plan) in plans.iter().enumerate() {
                if idle[index] || summary.categories[index].paused.is_some() {
                    continue;
                }
                if budget == 0 {
                    summary.budget_exhausted = true;
                    return;
                }
                let window = deps.settings.concurrency.min(budget);
                let claimed = match self
                    .claim_or_generate(plan, &owner, window, cutoff, &mut summary.categories[index])
                    .await
                {
                    Ok(claimed) => claimed,
                    Err(error) => return Self::storage_failure(summary, error, "claim"),
                };
                let (claimed, claim_started) = claimed;
                if claimed.is_empty() {
                    idle[index] = true;
                    continue;
                }
                progressed = true;
                let calls = self
                    .process_window(index, plan, &owner, claimed, claim_started, summary)
                    .await;
                budget = budget.saturating_sub(calls);
                if summary.aborted.is_some() || summary.drain_timeout {
                    return;
                }
            }
            if !progressed {
                return;
            }
        }
    }

    /// Claims a window; when nothing is claimable, generates up to one window
    /// of new rows first (ADR 0011 §5).
    async fn claim_or_generate(
        &self,
        plan: &CategoryPlan,
        owner: &str,
        window: u32,
        cutoff: OffsetDateTime,
        stats: &mut CategoryStats,
    ) -> Result<(Vec<ClaimedDecision>, Instant), Option<String>> {
        let deps = &self.deps;
        // The instant before the stored expiry is computed: row deadlines are
        // measured from here, so they are never later than that expiry.
        let claim = || async {
            let started = Instant::now();
            let now = OffsetDateTime::now_utc();
            let lease = now + time::Duration::seconds(i64::from(deps.settings.lease_seconds));
            Self::storage(deps.decision_repo.claim(
                &plan.identity,
                owner,
                lease,
                deps.settings.max_attempts,
                window,
                now,
            ))
            .await
            .map(|claimed| (claimed, started))
        };
        let (claimed, started) = claim().await?;
        if !claimed.is_empty() {
            stats.claimed += claimed.len() as u64;
            return Ok((claimed, started));
        }
        let generated = Self::storage(deps.decision_repo.generate_pending(
            &plan.identity,
            &deps.execution,
            cutoff,
            window,
            OffsetDateTime::now_utc(),
        ))
        .await?;
        stats.generated += generated;
        if generated == 0 {
            return Ok((Vec::new(), started));
        }
        let (claimed, started) = claim().await?;
        stats.claimed += claimed.len() as u64;
        Ok((claimed, started))
    }

    /// Dispatches every claimed row at once and joins them all. Returns the
    /// number of client calls made.
    async fn process_window(
        &self,
        index: usize,
        plan: &Arc<CategoryPlan>,
        owner: &Arc<str>,
        claimed: Vec<ClaimedDecision>,
        claim_started: Instant,
        summary: &mut DecisionRunSummary,
    ) -> u32 {
        let deps = &self.deps;
        summary.claimed += claimed.len() as u64;
        let calls_before = self.calls.load(Ordering::SeqCst);
        let lease = Duration::from_secs(u64::from(deps.settings.lease_seconds));
        let row_deadline = claim_started + lease - Duration::from_secs(5);
        let mut join_set = JoinSet::new();
        for row in claimed {
            join_set.spawn(task::process(task::TaskInput {
                deps: Arc::clone(deps),
                plan: Arc::clone(plan),
                category_index: index,
                row,
                owner: Arc::clone(owner),
                row_deadline,
                calls: Arc::clone(&self.calls),
            }));
        }
        let mut outcomes = Vec::new();
        let drained = tokio::time::timeout(lease, async {
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(outcome) => outcomes.push(outcome),
                    Err(error) => {
                        tracing::error!(stage = STAGE, "decision task failed: {error}");
                        summary.tasks_panicked += 1;
                    }
                }
            }
        })
        .await;
        if drained.is_err() {
            // Soft deadline: request cancellation, collect what finishes in
            // the grace period, leave the rest to lease reclaim.
            join_set.abort_all();
            let collected = tokio::time::timeout(CANCEL_GRACE, async {
                while let Some(result) = join_set.join_next().await {
                    match result {
                        Ok(outcome) => outcomes.push(outcome),
                        Err(error) => tracing::warn!(stage = STAGE, "cancelled task: {error}"),
                    }
                }
            })
            .await;
            if collected.is_err() {
                tracing::error!(
                    stage = STAGE,
                    "tasks still running after cancellation grace"
                );
            }
            summary.drain_timeout = true;
            summary.abort(Abort::Infrastructure {
                kind: "drain_timeout",
            });
        }
        if summary.tasks_panicked > 0 {
            summary.abort(Abort::Infrastructure {
                kind: "task_panicked",
            });
        }
        for outcome in outcomes {
            self.record(outcome, summary).await;
        }
        // Read after the join: includes calls whose task panicked or was
        // cancelled.
        let calls = self.calls.load(Ordering::SeqCst);
        summary.calls = calls;
        u32::try_from(calls - calls_before).unwrap_or(u32::MAX)
    }

    async fn record(&self, outcome: TaskOutcome, summary: &mut DecisionRunSummary) {
        let index = outcome.category_index;
        let category = summary.categories[index].category.clone();
        summary.input_tokens += outcome.input_tokens;
        match outcome.fault {
            Some("storage_timeout") => summary.storage_timeouts += 1,
            Some(_) => summary.storage_errors += 1,
            None => {}
        }
        if outcome.rate_limited {
            summary.rate_limited = true;
        }
        if outcome.pause && summary.categories[index].paused.is_none() {
            summary.categories[index].paused = Some(match outcome.status {
                TaskStatus::Refunded {
                    reason: "lease_insufficient",
                } => "lease_insufficient",
                _ => "retryable_failure",
            });
        }
        let row = Some(outcome.row_id);
        match &outcome.status {
            TaskStatus::Succeeded => {
                summary.categories[index].succeeded += 1;
                self.emit(
                    "decision_completed",
                    "info",
                    row,
                    "decision recorded",
                    json!({ "category": category, "input_tokens": outcome.input_tokens }),
                )
                .await;
            }
            TaskStatus::Failed { class, kind, state } => {
                if *state == rss_ai_news_storage::DecisionResultState::Failed {
                    summary.categories[index].failed += 1;
                } else {
                    summary.categories[index].retried += 1;
                }
                self.emit(
                    "decision_failed",
                    "warn",
                    row,
                    "decision attempt failed",
                    json!({
                        "category": category,
                        "error_kind": kind,
                        "class": class.as_str(),
                        "state": state.as_str(),
                        "retry_after_seconds": outcome.retry_after_seconds,
                    }),
                )
                .await;
            }
            TaskStatus::Refunded { reason } => {
                summary.categories[index].refunded += 1;
                let kind = if *reason == "lease_insufficient" {
                    "lease_insufficient"
                } else {
                    "decision_refunded"
                };
                self.emit(
                    kind,
                    "warn",
                    row,
                    "decision attempt refunded",
                    json!({
                        "category": category,
                        "reason": reason,
                        "retry_after_seconds": outcome.retry_after_seconds,
                    }),
                )
                .await;
            }
            TaskStatus::Unknown { kind } => {
                match *kind {
                    "owner_mismatch" => summary.owner_mismatch += 1,
                    "storage_timeout" => summary.storage_timeouts += 1,
                    _ => summary.release_failed += 1,
                }
                self.emit(
                    if *kind == "owner_mismatch" {
                        "owner_mismatch"
                    } else {
                        "decision_release_unknown"
                    },
                    "error",
                    row,
                    "decision row state unknown after release",
                    json!({ "category": category, "kind": kind }),
                )
                .await;
            }
        }
        if let Some(withheld) = outcome.raw_withheld {
            self.emit(
                "raw_response_withheld",
                "warn",
                row,
                "raw decision response not stored",
                json!({ "category": category, "reason": withheld }),
            )
            .await;
        }
        if let Some(abort) = outcome.abort {
            summary.abort(abort);
        }
    }
}
