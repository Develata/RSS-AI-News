//! DecisionRunFlow with a scripted DecisionClient on SQLite (ADR 0011 §6–§12).

mod common;

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use rss_ai_news_decision::{
    DecisionAnswer, DecisionClient, DecisionError, DecisionRequest, DecisionResponse, Exchange,
    Execution, Provider, QuestionAnswer, QuestionKind, QuestionSpec, RawInfo, RawResponsePolicy,
    SentState, StateTruncation, Usage, Withheld, canonical_policy, policy_version,
};
use rss_ai_news_runtime::{
    CategoryPlan, DecisionRunDeps, DecisionRunFlow, DecisionRunOptions, DecisionRunSettings,
    DecisionRunSummary, RunMeta,
};
use rss_ai_news_storage::{
    ClaimedDecision, DecisionArticle, DecisionExecution, DecisionFailure, DecisionIdentity,
    DecisionResultRepo, DecisionResultRepository, DecisionResultState, DecisionSuccess,
    RunEventRepo, StorageError, StoragePool,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{
    insert_config_rule, insert_source_with_category, make_test_pool, seed_extractor_rule_version,
    seed_pending_fetch_entry,
};

const EXECUTION_KEY: &str = "laya:multilingual:2048";
const BODY: &str = "正文里有一段不应出现在事件里的文字";

enum Reply {
    Ok,
    Err(DecisionError),
    /// The client gave up before sending (deadline already passed).
    Unsent,
    /// 2xx body that failed validation, with a capturable raw body.
    InvalidWithRaw,
    Hang,
    Panic,
}

struct ScriptedClient {
    replies: Mutex<VecDeque<Reply>>,
    calls: AtomicUsize,
}

impl ScriptedClient {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn sent() -> Option<SentState> {
    Some(SentState {
        state_sha256: "ab".repeat(32),
        body_chars: 7,
        body_truncated: false,
    })
}

fn raw(body: Option<&str>, withheld: Option<Withheld>) -> Option<RawInfo> {
    Some(RawInfo {
        bytes: 42,
        sha256: Some("cd".repeat(32)),
        body: body.map(|body| body.as_bytes().to_vec()),
        withheld,
    })
}

fn ok_response() -> DecisionResponse {
    DecisionResponse {
        execution: Execution {
            provider: Provider::Laya,
            requested_model: "multilingual".to_string(),
            reported_model: "laya-rl-agent".to_string(),
            routing_model: Some("multilingual".to_string()),
        },
        answers: vec![QuestionAnswer {
            id: "q_ad".to_string(),
            answer: DecisionAnswer::Noul {
                p_yes: 0.25,
                top_probability: Some(0.75),
            },
        }],
        usage: Usage {
            input_tokens: 100,
            state: StateTruncation::Laya {
                truncated: false,
                state_tokens_dropped: 0,
            },
        },
    }
}

#[async_trait]
impl DecisionClient for ScriptedClient {
    async fn decide(&self, request: &DecisionRequest<'_>, _deadline: Instant) -> Exchange {
        assert_eq!(request.body, BODY);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Ok);
        match reply {
            Reply::Ok => Exchange {
                result: Ok(ok_response()),
                sent: sent(),
                raw: raw(Some("{\"ok\":true}"), None),
            },
            Reply::Err(error) => Exchange {
                result: Err(error),
                sent: sent(),
                raw: None,
            },
            Reply::Unsent => Exchange {
                result: Err(DecisionError::Timeout { seconds: 0 }),
                sent: None,
                raw: None,
            },
            Reply::InvalidWithRaw => Exchange {
                result: Err(DecisionError::InvalidResponse {
                    reason: "probabilities do not sum to 1".to_string(),
                }),
                sent: sent(),
                raw: raw(Some("{\"bad\":true}"), None),
            },
            Reply::Hang => {
                tokio::time::sleep(Duration::from_secs(600)).await;
                unreachable!("cancelled before it wakes")
            }
            Reply::Panic => panic!("scripted panic"),
        }
    }
}

fn questions() -> Vec<QuestionSpec> {
    vec![QuestionSpec {
        id: "q_ad".to_string(),
        instructions: "Is this an ad?".to_string(),
        kind: QuestionKind::Noul {
            yes: None,
            no: None,
        },
    }]
}

fn plan(category: &str, questions: Vec<QuestionSpec>) -> CategoryPlan {
    CategoryPlan {
        identity: DecisionIdentity {
            category_key: category.to_string(),
            policy_version: policy_version(&questions, 6000),
            execution_key: EXECUTION_KEY.to_string(),
        },
        canonical_policy_json: canonical_policy(&questions, 6000),
        questions,
    }
}

fn settings() -> DecisionRunSettings {
    DecisionRunSettings {
        concurrency: 2,
        lease_seconds: 120,
        max_attempts: 3,
        request_timeout: Duration::from_secs(10),
        raw_response: RawResponsePolicy::OnFailure,
        raw_response_ttl_days: 30,
    }
}

/// Storage faults to inject around the real SQLite repository.
#[derive(Default)]
struct Faults {
    release_error: bool,
    owner_mismatch: bool,
    article_error: bool,
    article_timeout: bool,
    article_hang: bool,
}

struct FaultyRepo {
    inner: DecisionResultRepo,
    faults: Faults,
}

fn injected() -> StorageError {
    StorageError::Unavailable("injected".to_string())
}

#[async_trait]
impl DecisionResultRepository for FaultyRepo {
    async fn upsert_policy(
        &self,
        policy_version: &str,
        canonical_json: &str,
        now: OffsetDateTime,
    ) -> Result<(), StorageError> {
        self.inner
            .upsert_policy(policy_version, canonical_json, now)
            .await
    }
    async fn generate_pending(
        &self,
        identity: &DecisionIdentity,
        execution: &DecisionExecution,
        cutoff: OffsetDateTime,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        self.inner
            .generate_pending(identity, execution, cutoff, limit, now)
            .await
    }
    async fn claim(
        &self,
        identity: &DecisionIdentity,
        owner: &str,
        lease_expires_at: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedDecision>, StorageError> {
        self.inner
            .claim(identity, owner, lease_expires_at, max_attempts, limit, now)
            .await
    }
    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        success: &DecisionSuccess,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        if self.faults.release_error {
            return Err(injected());
        }
        if self.faults.owner_mismatch {
            return Ok(false);
        }
        self.inner.release_success(id, owner, success, now).await
    }
    async fn release_failure(
        &self,
        id: i64,
        owner: &str,
        failure: &DecisionFailure,
        now: OffsetDateTime,
    ) -> Result<Option<DecisionResultState>, StorageError> {
        if self.faults.release_error {
            return Err(injected());
        }
        if self.faults.owner_mismatch {
            return Ok(None);
        }
        self.inner.release_failure(id, owner, failure, now).await
    }
    async fn reclaim_expired(
        &self,
        identity: &DecisionIdentity,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        self.inner.reclaim_expired(identity, now).await
    }
    async fn terminalize_exhausted(
        &self,
        identity: &DecisionIdentity,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        self.inner
            .terminalize_exhausted(identity, max_attempts, now)
            .await
    }
    async fn purge_expired_raw(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<u64, StorageError> {
        self.inner.purge_expired_raw(now, limit).await
    }
    async fn load_article(&self, article_id: i64) -> Result<Option<DecisionArticle>, StorageError> {
        if self.faults.article_error {
            return Err(injected());
        }
        if self.faults.article_timeout {
            return Err(StorageError::Timeout);
        }
        if self.faults.article_hang {
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
        self.inner.load_article(article_id).await
    }
    async fn count_other_identity_pending(
        &self,
        identity: &DecisionIdentity,
    ) -> Result<u64, StorageError> {
        self.inner.count_other_identity_pending(identity).await
    }
}

fn deps(
    pool: &SqlitePool,
    client: Arc<ScriptedClient>,
    settings: DecisionRunSettings,
    faults: Faults,
) -> Arc<DecisionRunDeps> {
    let storage = StoragePool::Sqlite(pool.clone());
    let repo = Arc::new(FaultyRepo {
        inner: DecisionResultRepo::new_with_storage(storage.clone()),
        faults,
    });
    Arc::new(DecisionRunDeps {
        run: RunMeta::default(),
        settings,
        client,
        endpoint_origin: "https://laya.example".to_string(),
        execution: DecisionExecution {
            provider: "laya".to_string(),
            requested_model: "multilingual".to_string(),
            laya_max_len: Some(2048),
        },
        execution_key: EXECUTION_KEY.to_string(),
        decision_repo: repo,
        event_repo: Arc::new(RunEventRepo::new_with_storage(storage)),
    })
}

async fn seed(pool: &SqlitePool, category: &str, count: usize) {
    let extractor = seed_extractor_rule_version(pool).await;
    let config = insert_config_rule(pool).await;
    let source = insert_source_with_category(
        pool,
        config,
        category,
        &format!("source-{category}"),
        &format!("https://example.com/{category}.xml"),
    )
    .await;
    for index in 0..count {
        let entry = seed_pending_fetch_entry(
            pool,
            source,
            &format!("uid-{category}-{index}"),
            &format!("hash-{category}-{index}"),
            None,
        )
        .await;
        sqlx::query(
            "INSERT INTO articles (content_hash, canonical_link, title, body_text, extractor_strategy, \
             extractor_version, content_quality, word_count, origin_feed_entry_id, state) \
             VALUES (?, ?, 'title', ?, 'readability', ?, 'high', 1, ?, 'persisted')",
        )
        .bind(format!("content-{category}-{index}"))
        .bind(format!("https://example.com/{category}/{index}"))
        .bind(BODY)
        .bind(extractor)
        .bind(entry)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn run(
    pool: &SqlitePool,
    client: Arc<ScriptedClient>,
    settings: DecisionRunSettings,
    categories: Vec<CategoryPlan>,
    max_requests: u32,
) -> DecisionRunSummary {
    run_with(
        pool,
        client,
        settings,
        categories,
        max_requests,
        Faults::default(),
    )
    .await
}

async fn run_with(
    pool: &SqlitePool,
    client: Arc<ScriptedClient>,
    settings: DecisionRunSettings,
    categories: Vec<CategoryPlan>,
    max_requests: u32,
    faults: Faults,
) -> DecisionRunSummary {
    DecisionRunFlow::new(deps(pool, client, settings, faults))
        .run(&DecisionRunOptions {
            categories,
            max_requests,
            window_hours: 48,
        })
        .await
}

async fn states(pool: &SqlitePool) -> Vec<(String, i64)> {
    sqlx::query_as("SELECT state, attempt_count FROM decision_results ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn successful_rows_store_answers_and_events_without_content() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 3).await;
    let client = ScriptedClient::new([]);
    let summary = run(
        &pool,
        client.clone(),
        settings(),
        vec![plan("ai", questions())],
        10,
    )
    .await;
    assert_eq!(summary.exit_class(), 0);
    assert_eq!(
        (summary.calls, summary.claimed, summary.input_tokens),
        (3, 3, 300)
    );
    assert_eq!(summary.categories[0].succeeded, 3);
    assert_eq!(client.calls(), 3);
    let (answers, origin, raw): (String, String, Option<String>) = sqlx::query_as(
        "SELECT answers_json, endpoint_origin, raw_response FROM decision_results LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        answers,
        r#"{"answers":[{"id":"q_ad","p_yes":0.25,"top_probability":0.75,"type":"noul"}],"schema":1}"#
    );
    assert_eq!(origin, "https://laya.example");
    assert_eq!(raw, None, "on_failure keeps no raw body for a success");
    let events: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT event_kind, context_json FROM run_events WHERE stage = 'decision_run'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "decision_completed")
            .count(),
        3
    );
    assert!(
        events
            .iter()
            .all(|(_, context)| !context.as_deref().unwrap_or("").contains("不应出现"))
    );
    // Rerunning finds nothing new.
    let again = run(
        &pool,
        ScriptedClient::new([]),
        settings(),
        vec![plan("ai", questions())],
        10,
    )
    .await;
    assert_eq!(again.calls, 0);
}

#[tokio::test]
async fn the_request_budget_caps_calls() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 5).await;
    let client = ScriptedClient::new([]);
    let summary = run(
        &pool,
        client.clone(),
        settings(),
        vec![plan("ai", questions())],
        3,
    )
    .await;
    assert_eq!((summary.calls, client.calls()), (3, 3));
    assert!(summary.budget_exhausted);
    // Generation follows the windows: no backlog of pending rows.
    assert_eq!(states(&pool).await.len(), 3);
}

#[tokio::test]
async fn provider_wide_failures_refund_and_stop_with_exit_78() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 4).await;
    let client = ScriptedClient::new([
        Reply::Err(DecisionError::Unauthorized { status: 401 }),
        Reply::Err(DecisionError::Unauthorized { status: 401 }),
    ]);
    let summary = run(
        &pool,
        client.clone(),
        settings(),
        vec![plan("ai", questions())],
        10,
    )
    .await;
    assert_eq!(summary.exit_class(), 78);
    assert_eq!(client.calls(), 2, "one window, then the run stops");
    assert_eq!(
        states(&pool).await,
        vec![("pending".to_string(), 0), ("pending".to_string(), 0)]
    );
    assert_eq!(summary.categories[0].refunded, 2);
}

#[tokio::test]
async fn rate_limits_stop_the_run_but_exit_zero() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 2).await;
    let client = ScriptedClient::new([
        Reply::Err(DecisionError::RateLimited {
            retry_after_seconds: Some(30),
        }),
        Reply::Ok,
    ]);
    let summary = run(&pool, client, settings(), vec![plan("ai", questions())], 10).await;
    assert!(summary.rate_limited);
    assert_eq!(summary.exit_class(), 0);
    let states = states(&pool).await;
    assert!(states.contains(&("pending".to_string(), 0)));
    assert!(states.contains(&("succeeded".to_string(), 1)));
}

#[tokio::test]
async fn item_retryable_failures_charge_and_pause_the_category() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 4).await;
    let client = ScriptedClient::new([
        Reply::Err(DecisionError::Timeout { seconds: 10 }),
        Reply::Ok,
    ]);
    let summary = run(
        &pool,
        client.clone(),
        settings(),
        vec![plan("ai", questions())],
        10,
    )
    .await;
    assert_eq!(summary.exit_class(), 0);
    assert_eq!(summary.categories[0].paused, Some("retryable_failure"));
    assert_eq!(client.calls(), 2);
    let states = states(&pool).await;
    assert!(states.contains(&("pending".to_string(), 1)));
}

#[tokio::test]
async fn suspected_configuration_errors_charge_the_trigger_and_exit_1() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 1).await;
    let client = ScriptedClient::new([Reply::Err(DecisionError::HttpStatus { status: 404 })]);
    let summary = run(&pool, client, settings(), vec![plan("ai", questions())], 10).await;
    assert_eq!(summary.exit_class(), 1);
    assert_eq!(states(&pool).await, vec![("pending".to_string(), 1)]);
}

#[tokio::test]
async fn raw_bodies_follow_the_policy() {
    for (policy, reply, expected) in [
        (
            RawResponsePolicy::OnFailure,
            Reply::InvalidWithRaw,
            Some("{\"bad\":true}"),
        ),
        (RawResponsePolicy::Never, Reply::InvalidWithRaw, None),
        (RawResponsePolicy::Always, Reply::Ok, Some("{\"ok\":true}")),
    ] {
        let (_dir, pool) = make_test_pool().await;
        seed(&pool, "ai", 1).await;
        let mut settings = settings();
        settings.raw_response = policy;
        run(
            &pool,
            ScriptedClient::new([reply]),
            settings,
            vec![plan("ai", questions())],
            10,
        )
        .await;
        let (raw, bytes, expires): (Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT raw_response, raw_response_bytes, raw_response_expires_at FROM decision_results",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(raw.as_deref(), expected, "{policy:?}");
        assert_eq!(bytes, Some(42), "the fingerprint is kept either way");
        assert_eq!(expires.is_some(), expected.is_some());
    }
}

#[tokio::test]
async fn every_backlogged_category_gets_a_first_round_window() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 5).await;
    seed(&pool, "ml", 5).await;
    let summary = run(
        &pool,
        ScriptedClient::new([]),
        settings(),
        vec![plan("ai", questions()), plan("ml", questions())],
        4,
    )
    .await;
    assert_eq!(summary.calls, 4);
    assert_eq!(summary.categories[0].succeeded, 2);
    assert_eq!(summary.categories[1].succeeded, 2);
}

#[tokio::test]
async fn a_changed_policy_never_executes_rows_of_the_old_one() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 2).await;
    let old = plan("ai", questions());
    // Leave the old identity's rows pending.
    run(
        &pool,
        ScriptedClient::new([
            Reply::Err(DecisionError::Timeout { seconds: 1 }),
            Reply::Err(DecisionError::Timeout { seconds: 1 }),
        ]),
        settings(),
        vec![old.clone()],
        2,
    )
    .await;
    let mut changed = questions();
    changed[0].instructions = "Is this sponsored?".to_string();
    let summary = run(
        &pool,
        ScriptedClient::new([]),
        settings(),
        vec![plan("ai", changed)],
        10,
    )
    .await;
    assert_eq!(summary.categories[0].other_identity_pending, 2);
    assert_eq!(summary.categories[0].succeeded, 2);
    let old_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM decision_results WHERE policy_version = ? AND state = 'pending'",
    )
    .bind(&old.identity.policy_version)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old_rows, 2);
}

#[tokio::test]
async fn too_little_lease_left_refunds_without_calling() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 1).await;
    let mut settings = settings();
    settings.lease_seconds = 30;
    settings.request_timeout = Duration::from_secs(30);
    let client = ScriptedClient::new([]);
    let summary = run(
        &pool,
        client.clone(),
        settings,
        vec![plan("ai", questions())],
        10,
    )
    .await;
    assert_eq!((client.calls(), summary.calls), (0, 0));
    assert_eq!(summary.categories[0].paused, Some("lease_insufficient"));
    assert_eq!(states(&pool).await, vec![("pending".to_string(), 0)]);
}

#[tokio::test]
async fn a_panicking_task_stops_the_run_and_leaves_the_row_to_reclaim() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 1).await;
    let summary = run(
        &pool,
        ScriptedClient::new([Reply::Panic]),
        settings(),
        vec![plan("ai", questions())],
        10,
    )
    .await;
    assert_eq!(summary.tasks_panicked, 1);
    assert_eq!(
        summary.calls, 1,
        "the call is counted before the client runs"
    );
    assert_eq!(summary.exit_class(), 1);
    assert_eq!(states(&pool).await, vec![("running".to_string(), 1)]);
}

#[tokio::test]
async fn a_hanging_call_is_cancelled_at_the_drain_deadline() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 1).await;
    let mut settings = settings();
    settings.lease_seconds = 12;
    settings.request_timeout = Duration::from_secs(1);
    let started = Instant::now();
    let summary = run(
        &pool,
        ScriptedClient::new([Reply::Hang]),
        settings,
        vec![plan("ai", questions())],
        10,
    )
    .await;
    assert!(summary.drain_timeout);
    assert_eq!(summary.calls, 1, "a cancelled call still spent budget");
    assert_eq!(summary.exit_class(), 1);
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(states(&pool).await, vec![("running".to_string(), 1)]);
}

#[tokio::test]
async fn an_unsent_call_still_spends_budget() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 2).await;
    seed(&pool, "ml", 3).await;
    let mut settings = settings();
    settings.concurrency = 1;
    let client = ScriptedClient::new([Reply::Unsent]);
    let summary = run(
        &pool,
        client.clone(),
        settings,
        vec![plan("ai", questions()), plan("ml", questions())],
        2,
    )
    .await;
    assert_eq!((summary.calls, client.calls()), (2, 2));
}

#[tokio::test]
async fn a_failed_release_after_a_rate_limit_exits_1() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 1).await;
    let summary = run_with(
        &pool,
        ScriptedClient::new([Reply::Err(DecisionError::RateLimited {
            retry_after_seconds: None,
        })]),
        settings(),
        vec![plan("ai", questions())],
        10,
        Faults {
            release_error: true,
            ..Faults::default()
        },
    )
    .await;
    assert!(summary.rate_limited);
    assert_eq!(summary.release_failed, 1);
    assert_eq!(summary.exit_class(), 1);
}

#[tokio::test]
async fn an_owner_mismatch_stops_the_run_without_writing() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 3).await;
    let summary = run_with(
        &pool,
        ScriptedClient::new([]),
        settings(),
        vec![plan("ai", questions())],
        10,
        Faults {
            owner_mismatch: true,
            ..Faults::default()
        },
    )
    .await;
    assert_eq!(summary.owner_mismatch, 2, "one window, then the run stops");
    assert_eq!(summary.exit_class(), 1);
    assert_eq!(
        states(&pool).await,
        vec![("running".to_string(), 1), ("running".to_string(), 1)]
    );
}

#[tokio::test]
async fn an_article_read_error_refunds_and_counts_a_storage_error() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 1).await;
    let client = ScriptedClient::new([]);
    let summary = run_with(
        &pool,
        client.clone(),
        settings(),
        vec![plan("ai", questions())],
        10,
        Faults {
            article_error: true,
            ..Faults::default()
        },
    )
    .await;
    assert_eq!((summary.storage_errors, client.calls()), (1, 0));
    assert_eq!(summary.exit_class(), 1);
    assert_eq!(states(&pool).await, vec![("pending".to_string(), 0)]);
}

#[tokio::test]
async fn an_article_read_timeout_leaves_the_row_unknown() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 1).await;
    let summary = run_with(
        &pool,
        ScriptedClient::new([]),
        settings(),
        vec![plan("ai", questions())],
        10,
        Faults {
            article_hang: true,
            ..Faults::default()
        },
    )
    .await;
    assert_eq!(summary.storage_timeouts, 1);
    assert_eq!(summary.exit_class(), 1);
    assert_eq!(states(&pool).await, vec![("running".to_string(), 1)]);
}

#[tokio::test]
async fn a_timeout_reported_by_storage_is_unknown_not_refunded() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 1).await;
    let summary = run_with(
        &pool,
        ScriptedClient::new([]),
        settings(),
        vec![plan("ai", questions())],
        10,
        Faults {
            article_timeout: true,
            ..Faults::default()
        },
    )
    .await;
    assert_eq!((summary.storage_timeouts, summary.storage_errors), (1, 0));
    assert_eq!(states(&pool).await, vec![("running".to_string(), 1)]);
}

#[tokio::test]
async fn each_run_counts_its_own_calls() {
    let (_dir, pool) = make_test_pool().await;
    seed(&pool, "ai", 2).await;
    let flow = DecisionRunFlow::new(deps(
        &pool,
        ScriptedClient::new([]),
        settings(),
        Faults::default(),
    ));
    let options = DecisionRunOptions {
        categories: vec![plan("ai", questions())],
        max_requests: 1,
        window_hours: 48,
    };
    assert_eq!(flow.run(&options).await.calls, 1);
    let second = flow.run(&options).await;
    assert_eq!(second.calls, 1);
    assert_eq!(second.categories[0].succeeded, 1);
}
