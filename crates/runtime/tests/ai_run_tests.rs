mod common;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_ai::{AiClient, AiError, AiResponse, AiTask, TokenUsage};
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::Score0To100;
use rss_ai_news_runtime::{AiRunFlow, AiRunOptions, AiTaskStatus};
use rss_ai_news_storage::StorageError;
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};
use tokio::sync::Mutex;

use common::{app_config, make_test_pool, seed_persisted_article};

#[tokio::test]
async fn task_gen_inserts_pending_and_advances_article_to_ai_pending() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-task-gen-1", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), client);

    let summary = flow.task_gen(&opts()).await;

    assert_eq!(summary.scanned, 1);
    assert_eq!(summary.inserted, 1);
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");
    assert_eq!(
        ai_result_state_by_article(&pool, article_id).await,
        Some("pending".to_string())
    );
}

#[tokio::test]
async fn task_gen_skips_articles_already_advanced() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-task-gen-2", "title", "body").await;
    sqlx::query("UPDATE articles SET state = 'ai_pending' WHERE id = ?")
        .bind(article_id)
        .execute(&pool)
        .await
        .expect("article state should update");
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), client);

    let summary = flow.task_gen(&opts()).await;

    assert_eq!(summary.scanned, 0);
    assert_eq!(summary.inserted, 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM article_ai_results")
        .fetch_one(&pool)
        .await
        .expect("count should be readable");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn task_gen_only_scans_requested_category() {
    let (_dir, pool) = make_test_pool().await;
    let ai_article = seed_persisted_article(&pool, "ai-task-gen-cat-ai", "ai title", "body").await;
    let other_article =
        seed_persisted_article(&pool, "ai-task-gen-cat-other", "other title", "body").await;
    set_article_category(&pool, other_article, "other").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), client);

    let summary = flow.task_gen(&opts_for_category("other")).await;

    assert_eq!(summary.scanned, 1);
    assert_eq!(summary.inserted, 1);
    assert_eq!(article_state(&pool, ai_article).await, "persisted");
    assert_eq!(article_state(&pool, other_article).await, "ai_pending");
    assert_eq!(ai_result_count_by_article(&pool, ai_article).await, 0);
    assert_eq!(ai_result_count_by_article(&pool, other_article).await, 1);
}

#[tokio::test]
async fn process_succeeds_high_score_advances_article_to_ready_for_publish() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-process-1", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client.insert_success(ai_result_id, output_json(80)).await;

    let summary = flow.process_ai_tasks(&opts()).await;

    assert_eq!(summary.claimed, 1);
    assert_eq!(summary.succeeded, 1);
    assert_eq!(article_state(&pool, article_id).await, "ready_for_publish");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "succeeded");
}

#[tokio::test]
async fn process_succeeds_low_score_advances_article_to_ai_done() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-process-2", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client.insert_success(ai_result_id, output_json(10)).await;

    let summary = flow.process_ai_tasks(&opts()).await;

    assert_eq!(summary.claimed, 1);
    assert_eq!(summary.succeeded, 1);
    assert_eq!(article_state(&pool, article_id).await, "ai_done");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "succeeded");
}

#[tokio::test]
async fn process_filtered_advances_article_to_publish_skipped() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-process-3", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client
        .insert_success(
            ai_result_id,
            r#"{"keep_decision":false,"filter_reason":"not relevant"}"#.to_string(),
        )
        .await;

    let summary = flow.process_ai_tasks(&opts()).await;

    assert_eq!(summary.claimed, 1);
    assert_eq!(summary.filtered, 1);
    assert_eq!(article_state(&pool, article_id).await, "publish_skipped");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "filtered");
}

#[tokio::test]
async fn process_falls_back_to_next_model_when_primary_fails() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-fallback-1", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    let mut opts = opts();
    opts.fallback_models = vec!["fallback-model".to_string()];
    flow.task_gen(&opts).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    // primary(test-model) 命中 quota（should_fallback=true）→ fallback-model 成功。
    client
        .insert_error(
            ai_result_id,
            AiError::QuotaExceeded {
                message: "no quota".to_string(),
            },
        )
        .await;
    client.insert_success(ai_result_id, output_json(80)).await;

    let summary = flow.process_ai_tasks(&opts).await;

    assert_eq!(summary.claimed, 1);
    assert_eq!(summary.succeeded, 1);
    assert_eq!(article_state(&pool, article_id).await, "ready_for_publish");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "succeeded");
    // model_id（幂等键）锚定主模型；effective_model_id 记实际成功的 fallback 模型。
    let (model_id, effective): (String, Option<String>) =
        sqlx::query_as("SELECT model_id, effective_model_id FROM article_ai_results WHERE id = ?")
            .bind(ai_result_id)
            .fetch_one(&pool)
            .await
            .expect("ai result row readable");
    assert_eq!(model_id, "test-model");
    assert_eq!(effective.as_deref(), Some("fallback-model"));
}

#[tokio::test]
async fn process_does_not_fall_back_on_connection_error() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-fallback-2", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    let mut opts = opts();
    opts.fallback_models = vec!["fallback-model".to_string()];
    flow.task_gen(&opts).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    // primary ConnectionFailed（should_fallback=false）→ 不试 fallback，retryable 回 pending。
    client
        .insert_error(ai_result_id, AiError::ConnectionFailed("down".to_string()))
        .await;
    // 这条成功响应不应被消费（fallback 未触发）。
    client.insert_success(ai_result_id, output_json(80)).await;

    let summary = flow.process_ai_tasks(&opts).await;

    assert_eq!(summary.succeeded, 0);
    assert_eq!(summary.retryable_failed, 1);
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "pending");
}

#[tokio::test]
async fn process_writes_ai_raw_response_artifact_before_release() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-process-4", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client.insert_success(ai_result_id, output_json(90)).await;

    let summary = flow.process_ai_tasks(&opts()).await;
    let (artifact_id, linked_artifact_id): (i64, Option<i64>) = sqlx::query_as(
        r#"
        SELECT raw_artifacts.id, article_ai_results.raw_response_artifact_id
        FROM raw_artifacts
        JOIN article_ai_results
          ON article_ai_results.raw_response_artifact_id = raw_artifacts.id
        WHERE raw_artifacts.kind = 'ai_raw_response'
          AND raw_artifacts.artifact_key = ?
          AND article_ai_results.id = ?
        "#,
    )
    .bind(ai_result_id.to_string())
    .bind(ai_result_id)
    .fetch_one(&pool)
    .await
    .expect("AI raw response artifact should be linked after release");

    assert_eq!(summary.succeeded, 1);
    assert_eq!(Some(artifact_id), linked_artifact_id);
}

#[tokio::test]
async fn process_only_claims_requested_category() {
    let (_dir, pool) = make_test_pool().await;
    let ai_article = seed_persisted_article(&pool, "ai-process-cat-ai", "ai title", "body").await;
    let other_article =
        seed_persisted_article(&pool, "ai-process-cat-other", "other title", "body").await;
    set_article_category(&pool, other_article, "other").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts_for_category("ai")).await;
    flow.task_gen(&opts_for_category("other")).await;
    let ai_result_id = ai_result_id_by_article(&pool, ai_article).await;
    let other_result_id = ai_result_id_by_article(&pool, other_article).await;
    client.insert_success(ai_result_id, output_json(80)).await;

    let summary = flow.process_ai_tasks(&opts_for_category("ai")).await;

    assert_eq!(summary.claimed, 1);
    assert_eq!(summary.succeeded, 1);
    assert_eq!(article_state(&pool, ai_article).await, "ready_for_publish");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "succeeded");
    assert_eq!(article_state(&pool, other_article).await, "ai_pending");
    assert_eq!(ai_result_state(&pool, other_result_id).await, "pending");
}

#[tokio::test]
async fn process_releases_retryable_on_5xx_error() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-process-5", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client
        .insert_error(
            ai_result_id,
            AiError::HttpStatus {
                code: 503,
                message: "service unavailable".to_string(),
            },
        )
        .await;

    let summary = flow.process_ai_tasks(&opts()).await;
    let (state, attempts): (String, i64) =
        sqlx::query_as("SELECT state, attempt_count FROM article_ai_results WHERE id = ?")
            .bind(ai_result_id)
            .fetch_one(&pool)
            .await
            .expect("AI result should be readable");

    assert_eq!(summary.retryable_failed, 1);
    assert_eq!(state, "pending");
    assert_eq!(attempts, 1);
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");
}

#[tokio::test]
async fn process_releases_permanent_on_invalid_json() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-process-6", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client
        .insert_success(ai_result_id, "not json".to_string())
        .await;

    let summary = flow.process_ai_tasks(&opts()).await;

    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(
        ai_result_state(&pool, ai_result_id).await,
        "permanent_failed"
    );
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");
}

// === W15: 重试预算耗尽闭环（docs/plan/15 §3/§5） ===

#[tokio::test]
async fn process_retryable_failure_on_final_attempt_folds_to_permanent_failed() {
    // W15 §3：max_attempts=1 ⇒ 本次 claim 自增到 1 即预算内最后一次尝试，
    // retryable 5xx 由 release SQL 内折叠 → permanent_failed。
    // 改前语义（RED）：回 pending 但 claim 过滤 attempt_count < max 永远捞不回，卡死。
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-exhaust-fold", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    let mut opts = opts();
    opts.max_attempts = 1;
    flow.task_gen(&opts).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client
        .insert_error(
            ai_result_id,
            AiError::HttpStatus {
                code: 503,
                message: "service unavailable".to_string(),
            },
        )
        .await;

    let summary = flow.process_ai_tasks(&opts).await;

    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(summary.retryable_failed, 0);
    let (state, error_kind): (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error_kind FROM article_ai_results WHERE id = ?")
            .bind(ai_result_id)
            .fetch_one(&pool)
            .await
            .expect("AI result should be readable");
    assert_eq!(state, "permanent_failed");
    // 折叠保留真实错误，不被 sweep 的兜底文案覆盖。
    assert!(error_kind.is_some());
    // article 保持 ai_pending（08 §5.2：permanent_failed 不推进 article）。
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");
    // 事件升 error 级并带 budget_exhausted（15 §7）。
    let (severity, context): (String, String) = sqlx::query_as(
        "SELECT severity, context_json FROM run_events WHERE event_kind = 'ai_failed'",
    )
    .fetch_one(&pool)
    .await
    .expect("ai_failed event should exist");
    assert_eq!(severity, "error");
    assert!(context.contains(r#""budget_exhausted":true"#));
}

#[tokio::test]
async fn process_run_start_maintenance_reclaims_then_sweeps_exhausted_row() {
    // W15 §5：入口顺序 ① reclaim（过期 running → pending）→ ② sweep（耗尽 → 终态）。
    // 模拟"最后一次尝试中崩溃"：running + lease 过期 + attempt_count = max(=3)，
    // 该行必须先被 reclaim 送回 pending，sweep 才能按谓词收走——单测两步接线与顺序。
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-exhaust-sweep", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    let expired = OffsetDateTime::now_utc() - Duration::seconds(120);
    sqlx::query(
        "UPDATE article_ai_results SET state = 'running', lease_owner = 'dead-owner', \
         lease_expires_at = ?, attempt_count = 3 WHERE id = ?",
    )
    .bind(expired)
    .bind(ai_result_id)
    .execute(&pool)
    .await
    .expect("seed crashed-final-attempt row");

    let summary = flow.process_ai_tasks(&opts()).await;

    // 行已被 sweep 转终态，claim 循环拿不到任何任务。
    assert_eq!(summary.claimed, 0);
    assert_eq!(
        ai_result_state(&pool, ai_result_id).await,
        "permanent_failed"
    );
    // maintenance 事件各一条：leases_reclaimed(info) / retry_budget_swept(warn)。
    let (reclaim_severity,): (String,) =
        sqlx::query_as("SELECT severity FROM run_events WHERE event_kind = 'leases_reclaimed'")
            .fetch_one(&pool)
            .await
            .expect("leases_reclaimed event should exist");
    assert_eq!(reclaim_severity, "info");
    let (swept_severity, swept_context): (String, String) = sqlx::query_as(
        "SELECT severity, context_json FROM run_events WHERE event_kind = 'retry_budget_swept'",
    )
    .fetch_one(&pool)
    .await
    .expect("retry_budget_swept event should exist");
    assert_eq!(swept_severity, "warn");
    assert!(swept_context.contains(r#""count":1"#));
}

#[tokio::test]
async fn process_run_start_maintenance_is_silent_when_nothing_to_do() {
    // W15 §7：影响行数 = 0 时 maintenance 静默，不产生事件噪音。
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-maintenance-quiet", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client.insert_success(ai_result_id, output_json(80)).await;

    let summary = flow.process_ai_tasks(&opts()).await;

    assert_eq!(summary.succeeded, 1);
    let maintenance_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_events WHERE event_kind IN ('leases_reclaimed', 'retry_budget_swept')",
    )
    .fetch_one(&pool)
    .await
    .expect("event count should be readable");
    assert_eq!(maintenance_events, 0);
}

fn flow(pool: SqlitePool, ai_client: Arc<MockAiClient>) -> AiRunFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let ctx = Arc::new(common::ai_deps(pool, app, ai_client));
    AiRunFlow::new(ctx)
}

fn opts() -> AiRunOptions {
    opts_for_category("ai")
}

fn opts_for_category(category_key: &str) -> AiRunOptions {
    AiRunOptions {
        task_gen_batch_size: 10,
        process_batch_size: 10,
        max_attempts: 3,
        prompt_template: "Title: {title}\nCategory: {category_key}\nBody: {body_text}"
            .to_string()
            .into(),
        model_id: "test-model".to_string(),
        fallback_models: Vec::new(),
        max_input_chars: 1024,
        max_tokens: 128,
        temperature: 0.0,
        min_importance_score: Score0To100::try_new(30).expect("0..=100"),
        max_batches: 0,
        category_key: category_key.to_string(),
        prompt_version: 1,
        output_schema_version: 1,
        candidate_window_hours: 48,
    }
}

fn output_json(score: i32) -> String {
    format!(
        r#"{{"summary":"summary","tags":["ai"],"importance_score":{score},"keep_decision":true}}"#
    )
}

#[derive(Default)]
struct MockAiClient {
    /// 每个 ai_result_id 一个响应队列：按 invoke 顺序 pop_front，可表达 fallback 链上
    /// 各模型尝试的不同结果（如 primary 失败 → fallback 成功）。
    responses: Mutex<HashMap<i64, VecDeque<MockAiResult>>>,
}

enum MockAiResult {
    Success(String),
    Error(AiError),
}

impl MockAiClient {
    async fn insert_success(&self, ai_result_id: i64, raw_response: String) {
        self.responses
            .lock()
            .await
            .entry(ai_result_id)
            .or_default()
            .push_back(MockAiResult::Success(raw_response));
    }

    async fn insert_error(&self, ai_result_id: i64, error: AiError) {
        self.responses
            .lock()
            .await
            .entry(ai_result_id)
            .or_default()
            .push_back(MockAiResult::Error(error));
    }
}

#[async_trait]
impl AiClient for MockAiClient {
    async fn invoke(&self, task: &AiTask) -> Result<AiResponse, AiError> {
        let response = self
            .responses
            .lock()
            .await
            .get_mut(&task.article_ai_result_id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| {
                MockAiResult::Error(AiError::ConnectionFailed(
                    "missing mock response".to_string(),
                ))
            });
        match response {
            MockAiResult::Success(raw_response) => Ok(AiResponse {
                article_ai_result_id: task.article_ai_result_id,
                raw_response,
                usage: Some(TokenUsage {
                    tokens_in: 11,
                    tokens_out: 22,
                    cost_micro_usd: None,
                }),
                latency_ms: 33,
            }),
            MockAiResult::Error(error) => Err(error),
        }
    }
}

async fn article_state(pool: &SqlitePool, article_id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM articles WHERE id = ?")
        .bind(article_id)
        .fetch_one(pool)
        .await
        .expect("article should be readable")
}

async fn ai_result_id_by_article(pool: &SqlitePool, article_id: i64) -> i64 {
    sqlx::query_scalar("SELECT id FROM article_ai_results WHERE article_id = ?")
        .bind(article_id)
        .fetch_one(pool)
        .await
        .expect("AI result should be readable")
}

async fn ai_result_state(pool: &SqlitePool, ai_result_id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM article_ai_results WHERE id = ?")
        .bind(ai_result_id)
        .fetch_one(pool)
        .await
        .expect("AI result should be readable")
}

async fn ai_result_state_by_article(pool: &SqlitePool, article_id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT state FROM article_ai_results WHERE article_id = ?")
        .bind(article_id)
        .fetch_optional(pool)
        .await
        .expect("AI result state should be readable")
}

async fn ai_result_count_by_article(pool: &SqlitePool, article_id: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM article_ai_results WHERE article_id = ?")
        .bind(article_id)
        .fetch_one(pool)
        .await
        .expect("AI result count should be readable")
}

async fn set_article_category(pool: &SqlitePool, article_id: i64, category_key: &str) {
    sqlx::query(
        r#"
        UPDATE feed_sources
        SET category_key = ?
        WHERE id = (
            SELECT fe.source_id
            FROM feed_entries fe
            JOIN articles a ON a.origin_feed_entry_id = fe.id
            WHERE a.id = ?
        )
        "#,
    )
    .bind(category_key)
    .bind(article_id)
    .execute(pool)
    .await
    .expect("article source category should update");
}

#[tokio::test]
async fn ai_live_tasks_are_bounded_by_concurrency() {
    struct ObserveTasks {
        deps: std::sync::OnceLock<std::sync::Weak<rss_ai_news_runtime::context::AiDeps>>,
        peak: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl AiClient for ObserveTasks {
        async fn invoke(&self, _: &AiTask) -> Result<AiResponse, AiError> {
            self.peak.fetch_max(
                self.deps.get().unwrap().strong_count(),
                std::sync::atomic::Ordering::SeqCst,
            );
            tokio::task::yield_now().await;
            Err(AiError::ConnectionFailed("expected failure".into()))
        }
    }
    let (_dir, pool) = make_test_pool().await;
    for i in 0..20 {
        seed_persisted_article(&pool, &format!("bounded-{i}"), "title", "body").await;
    }
    let client = Arc::new(ObserveTasks {
        deps: Default::default(),
        peak: Default::default(),
    });
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let deps = Arc::new(common::ai_deps(pool, app, client.clone()));
    client.deps.set(Arc::downgrade(&deps)).unwrap();
    let mut options = opts();
    options.task_gen_batch_size = 20;
    options.process_batch_size = 20;
    options.max_batches = 1;
    let summary = AiRunFlow::new(deps).run(options).await;
    assert_eq!(summary.process.claimed, 20);
    assert_eq!(summary.process.retryable_failed, 20);
    assert_eq!(summary.process.tasks_panicked, 0);
    let peak = client.peak.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        peak <= 3,
        "{peak} context owners: only flow + 2 live tasks allowed"
    );
}

#[tokio::test]
async fn oversized_ai_failures_remain_bounded_in_database_and_events() {
    for code in [400, 503] {
        let (_dir, pool) = make_test_pool().await;
        let article_id = seed_persisted_article(&pool, "large-error", "title", "body").await;
        let client = Arc::new(MockAiClient::default());
        let flow = flow(pool.clone(), client.clone());
        flow.task_gen(&opts()).await;
        let result_id = ai_result_id_by_article(&pool, article_id).await;
        client
            .insert_error(
                result_id,
                AiError::HttpStatus {
                    code,
                    message: "中文🦀".repeat(200_000),
                },
            )
            .await;
        let summary = flow.process_ai_tasks(&opts()).await;
        assert_eq!(summary.claimed, 1);
        assert_eq!(summary.retryable_failed, u32::from(code == 503));
        assert_eq!(summary.permanent_failed, u32::from(code == 400));
        let last_error: String =
            sqlx::query_scalar("SELECT last_error FROM article_ai_results WHERE id = ?")
                .bind(result_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let event: String = sqlx::query_scalar(
            "SELECT message FROM run_events WHERE event_kind = 'ai_failed' AND target_id = ?",
        )
        .bind(result_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        for diagnostic in [last_error, event] {
            assert!(diagnostic.len() <= 16 * 1024);
            assert!(diagnostic.contains("[truncated, original_bytes="));
        }
        assert_eq!(
            ai_result_state(&pool, result_id).await,
            if code == 503 {
                "pending"
            } else {
                "permanent_failed"
            }
        );
    }
}

// === C1: storage errors reading the article follow the retry contract ===

/// Delegates to the real repository; `find_by_id` fails with `error` for the
/// first `failures` calls.
struct FlakyArticleRepo {
    inner: rss_ai_news_storage::ArticleRepo,
    failures: std::sync::atomic::AtomicUsize,
    error: fn() -> StorageError,
}

#[async_trait]
impl rss_ai_news_storage::ArticleRepository for FlakyArticleRepo {
    async fn insert_or_get_by_content_hash(
        &self,
        article: &rss_ai_news_storage::NewArticle,
    ) -> Result<rss_ai_news_storage::ArticleInsertOutcome, StorageError> {
        self.inner.insert_or_get_by_content_hash(article).await
    }
    async fn persist_for_entry(
        &self,
        article: &rss_ai_news_storage::NewArticle,
        owner: &str,
        fallback: bool,
        now: OffsetDateTime,
    ) -> Result<rss_ai_news_storage::EntryPersistOutcome, StorageError> {
        self.inner
            .persist_for_entry(article, owner, fallback, now)
            .await
    }
    async fn find_by_id(
        &self,
        id: i64,
    ) -> Result<Option<rss_ai_news_domain::model::Article>, StorageError> {
        let left = self.failures.load(std::sync::atomic::Ordering::SeqCst);
        if left > 0 {
            self.failures
                .store(left - 1, std::sync::atomic::Ordering::SeqCst);
            return Err((self.error)());
        }
        self.inner.find_by_id(id).await
    }
    async fn list_persisted_for_ai_task_gen(
        &self,
        category_key: &str,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<rss_ai_news_storage::ArticleAiTaskCandidate>, StorageError> {
        self.inner
            .list_persisted_for_ai_task_gen(category_key, batch_size, after_id)
            .await
    }
    async fn list_fresh_for_ai_task_gen(
        &self,
        category_key: &str,
        identity: &rss_ai_news_storage::NewAiResult,
        published_since: OffsetDateTime,
        batch_size: u32,
    ) -> Result<Vec<rss_ai_news_storage::ArticleAiTaskCandidate>, StorageError> {
        self.inner
            .list_fresh_for_ai_task_gen(category_key, identity, published_since, batch_size)
            .await
    }
    async fn list_in_window_for_backfill(
        &self,
        date_from: Option<OffsetDateTime>,
        date_to: Option<OffsetDateTime>,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<rss_ai_news_storage::BackfillArticleCandidate>, StorageError> {
        self.inner
            .list_in_window_for_backfill(date_from, date_to, batch_size, after_id)
            .await
    }
    async fn list_for_content_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<rss_ai_news_storage::ContentHashReindexCandidate>, StorageError> {
        self.inner
            .list_for_content_hash_reindex(after_id, batch_size)
            .await
    }
    async fn list_content_hashes(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<rss_ai_news_storage::ArticleContentHash>, StorageError> {
        self.inner.list_content_hashes(after_id, batch_size).await
    }
    async fn update_content_hash(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<rss_ai_news_storage::UpdateContentHashOutcome, StorageError> {
        self.inner.update_content_hash(id, new_content_hash).await
    }
    async fn peek_content_hash_outcome(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<rss_ai_news_storage::UpdateContentHashOutcome, StorageError> {
        self.inner
            .peek_content_hash_outcome(id, new_content_hash)
            .await
    }
}

fn flaky_flow(
    pool: SqlitePool,
    client: Arc<MockAiClient>,
    failures: usize,
    error: fn() -> StorageError,
) -> AiRunFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let mut deps = common::ai_deps(pool.clone(), app, client);
    deps.article_repo = Arc::new(FlakyArticleRepo {
        inner: rss_ai_news_storage::ArticleRepo::new(pool),
        failures: std::sync::atomic::AtomicUsize::new(failures),
        error,
    });
    AiRunFlow::new(Arc::new(deps))
}

async fn ai_result_state_and_attempts(pool: &SqlitePool, ai_result_id: i64) -> (String, i64) {
    sqlx::query_as("SELECT state, attempt_count FROM article_ai_results WHERE id = ?")
        .bind(ai_result_id)
        .fetch_one(pool)
        .await
        .expect("AI result should be readable")
}

#[tokio::test]
async fn transient_article_read_error_returns_task_to_pending_with_budget_intact() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-read-transient", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flaky_flow(pool.clone(), Arc::clone(&client), 1, || {
        StorageError::Timeout
    });
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;

    let summary = flow.process_ai_tasks(&opts()).await;

    assert_eq!(summary.retryable_failed, 1);
    assert_eq!(summary.permanent_failed, 0);
    // The claim spent one attempt of three; the row is claimable again.
    assert_eq!(
        ai_result_state_and_attempts(&pool, ai_result_id).await,
        ("pending".to_string(), 1)
    );
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");

    // The next run reads the article and succeeds.
    client.insert_success(ai_result_id, output_json(80)).await;
    let summary = flow.process_ai_tasks(&opts()).await;
    assert_eq!(summary.succeeded, 1);
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "succeeded");
}

#[tokio::test]
async fn transient_article_read_error_on_last_attempt_is_permanent() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-read-last", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flaky_flow(pool.clone(), client, 1, || StorageError::Timeout);
    let mut opts = opts();
    opts.max_attempts = 1;
    flow.task_gen(&opts).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;

    let summary = flow.process_ai_tasks(&opts).await;

    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(summary.retryable_failed, 0);
    let (state, kind): (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error_kind FROM article_ai_results WHERE id = ?")
            .bind(ai_result_id)
            .fetch_one(&pool)
            .await
            .expect("AI result should be readable");
    assert_eq!(state, "permanent_failed");
    assert_eq!(kind.as_deref(), Some("db_timeout"));
}

#[tokio::test]
async fn non_retryable_article_read_error_is_permanent() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-read-corrupt", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let flow = flaky_flow(pool.clone(), client, 1, || {
        StorageError::Corruption("bad row".to_string())
    });
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;

    let summary = flow.process_ai_tasks(&opts()).await;

    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(
        ai_result_state_and_attempts(&pool, ai_result_id).await,
        ("permanent_failed".to_string(), 1)
    );
}

#[tokio::test]
async fn ai_run_uses_the_injected_clock() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-clock", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let mut deps = common::ai_deps(pool.clone(), app, Arc::clone(&client) as Arc<dyn AiClient>);
    let fixed = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::days(400);
    deps.clock = Arc::new(move || fixed);
    let flow = AiRunFlow::new(Arc::new(deps));
    flow.task_gen(&opts()).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client.insert_success(ai_result_id, output_json(80)).await;

    let summary = flow.process_ai_tasks(&opts()).await;

    assert_eq!(summary.succeeded, 1);
    let (started, completed): (OffsetDateTime, OffsetDateTime) =
        sqlx::query_as("SELECT started_at, completed_at FROM article_ai_results WHERE id = ?")
            .bind(ai_result_id)
            .fetch_one(&pool)
            .await
            .expect("AI result should be readable");
    assert_eq!(started, fixed);
    assert_eq!(completed, fixed);
}

// === C2: release outcomes are reported only when the transition landed ===

/// Returns `raw_response`, after optionally moving the row's lease to another
/// owner (as a concurrent reclaim + re-claim would).
struct LeaseStealingClient {
    pool: SqlitePool,
    raw_response: String,
    steal: bool,
}

#[async_trait]
impl AiClient for LeaseStealingClient {
    async fn invoke(&self, task: &AiTask) -> Result<AiResponse, AiError> {
        if self.steal {
            sqlx::query("UPDATE article_ai_results SET lease_owner = 'thief' WHERE id = ?")
                .bind(task.article_ai_result_id)
                .execute(&self.pool)
                .await
                .expect("lease should move");
        }
        Ok(AiResponse {
            article_ai_result_id: task.article_ai_result_id,
            raw_response: self.raw_response.clone(),
            usage: None,
            latency_ms: 1,
        })
    }
}

async fn run_with_client(
    pool: &SqlitePool,
    content_hash: &str,
    client: Arc<dyn AiClient>,
    max_attempts: u32,
) -> (i64, rss_ai_news_runtime::AiProcessSummary) {
    let article_id = seed_persisted_article(pool, content_hash, "title", "body").await;
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let flow = AiRunFlow::new(Arc::new(common::ai_deps(pool.clone(), app, client)));
    let mut opts = opts();
    opts.max_attempts = max_attempts;
    flow.task_gen(&opts).await;
    let ai_result_id = ai_result_id_by_article(pool, article_id).await;
    (ai_result_id, flow.process_ai_tasks(&opts).await)
}

/// Makes every UPDATE of an AI row into `state` fail inside the database.
async fn fail_updates_to_state(pool: &SqlitePool, state: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER fail_release BEFORE UPDATE ON article_ai_results \
         WHEN NEW.state = '{state}' AND OLD.state = 'running' \
         BEGIN SELECT RAISE(ABORT, 'injected release failure'); END;"
    )))
    .execute(pool)
    .await
    .expect("trigger should install");
}

async fn ai_failed_events(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM run_events WHERE event_kind = 'ai_failed'")
        .fetch_one(pool)
        .await
        .expect("events should be countable")
}

#[tokio::test]
async fn permanent_release_that_lost_the_lease_is_not_reported_as_permanent() {
    let (_dir, pool) = make_test_pool().await;
    let client = Arc::new(LeaseStealingClient {
        pool: pool.clone(),
        raw_response: "not json".to_string(),
        steal: true,
    });
    let (ai_result_id, summary) = run_with_client(&pool, "ai-perm-lost", client, 3).await;

    assert_eq!(summary.permanent_failed, 0);
    assert_eq!(summary.lease_lost, 1);
    assert_eq!(summary.release_failed, 0);
    assert_eq!(ai_failed_events(&pool).await, 0);
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "running");
}

#[tokio::test]
async fn permanent_release_storage_error_is_reported_as_release_failure() {
    let (_dir, pool) = make_test_pool().await;
    fail_updates_to_state(&pool, "permanent_failed").await;
    let client = Arc::new(LeaseStealingClient {
        pool: pool.clone(),
        raw_response: "not json".to_string(),
        steal: false,
    });
    let (ai_result_id, summary) = run_with_client(&pool, "ai-perm-err", client, 3).await;

    assert_eq!(summary.permanent_failed, 0);
    assert_eq!(summary.release_failed, 1);
    assert_eq!(ai_failed_events(&pool).await, 0);
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "running");
    let sample = &summary.failure_samples[0];
    assert_eq!(sample.status, AiTaskStatus::ReleaseFailed);
}

#[tokio::test]
async fn retryable_release_storage_error_is_reported_as_release_failure() {
    let (_dir, pool) = make_test_pool().await;
    fail_updates_to_state(&pool, "pending").await;
    let client = Arc::new(MockAiClient::default());
    // No queued response: the mock fails with a retryable connection error.
    let (ai_result_id, summary) = run_with_client(&pool, "ai-retry-err", client, 3).await;

    assert_eq!(summary.retryable_failed, 0);
    assert_eq!(summary.release_failed, 1);
    assert_eq!(ai_failed_events(&pool).await, 0);
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "running");
}

#[tokio::test]
async fn success_release_that_lost_the_lease_is_not_reported_as_permanent() {
    let (_dir, pool) = make_test_pool().await;
    let client = Arc::new(LeaseStealingClient {
        pool: pool.clone(),
        raw_response: output_json(80),
        steal: true,
    });
    let (ai_result_id, summary) = run_with_client(&pool, "ai-success-lost", client, 3).await;

    assert_eq!(summary.succeeded, 0);
    assert_eq!(summary.permanent_failed, 0);
    assert_eq!(summary.lease_lost, 1);
    assert_eq!(summary.failure_samples[0].status, AiTaskStatus::LeaseLost);
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "running");
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_events WHERE event_kind = 'ai_release_not_applied'",
    )
    .fetch_one(&pool)
    .await
    .expect("events should be countable");
    assert_eq!(events, 1);
}

// === C3: task_gen / backfill starvation ===

/// Inserts an AI row with the identity `opts()` uses (as an earlier backfill
/// with the same prompt version would), leaving the article `persisted`.
async fn seed_identity_row(
    pool: &SqlitePool,
    article_id: i64,
    state: &str,
    outcome: Option<(i32, bool)>,
) {
    sqlx::query(
        "INSERT INTO article_ai_results (article_id, prompt_version, output_schema_version, \
         model_id, state, importance_score, keep_decision, summary, tags_json) \
         VALUES (?, 1, 1, 'test-model', ?, ?, ?, 'summary', '[\"ai\"]')",
    )
    .bind(article_id)
    .bind(state)
    .bind(outcome.map(|(score, _)| score))
    .bind(outcome.map(|(_, keep)| i32::from(keep)))
    .execute(pool)
    .await
    .expect("identity row should insert");
}

#[tokio::test]
async fn task_gen_reconciles_conflicting_pending_row_and_reaches_newer_articles_with_batch_one() {
    let (_dir, pool) = make_test_pool().await;
    let first = seed_persisted_article(&pool, "starve-first", "title", "body").await;
    let second = seed_persisted_article(&pool, "starve-second", "title", "body").await;
    // Outside the publish window: this is the id-ordered (backlog) list.
    set_source_time(&pool, first, days_ago(10)).await;
    set_source_time(&pool, second, days_ago(10)).await;
    seed_identity_row(&pool, first, "pending", None).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));
    let mut opts = opts();
    opts.task_gen_batch_size = 1;

    let one = flow.task_gen(&opts).await;
    let two = flow.task_gen(&opts).await;

    assert_eq!((one.conflict_skipped, one.reconciled), (1, 1));
    assert_eq!(two.inserted, 1);
    assert_eq!(article_state(&pool, first).await, "ai_pending");
    assert_eq!(article_state(&pool, second).await, "ai_pending");
    assert_eq!(ai_result_count_by_article(&pool, first).await, 1);
    assert_eq!(ai_result_count_by_article(&pool, second).await, 1);
}

#[tokio::test]
async fn task_gen_moves_past_permanently_failed_rows_within_one_pass() {
    let (_dir, pool) = make_test_pool().await;
    let failed = seed_persisted_article(&pool, "starve-failed", "title", "body").await;
    let fresh = seed_persisted_article(&pool, "starve-fresh", "title", "body").await;
    set_source_time(&pool, failed, days_ago(10)).await;
    set_source_time(&pool, fresh, days_ago(10)).await;
    seed_identity_row(&pool, failed, "permanent_failed", None).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));
    let mut opts = opts();
    opts.task_gen_batch_size = 1;

    let summary = flow.task_gen(&opts).await;

    assert_eq!(summary.scanned, 2);
    assert_eq!(summary.inserted, 1);
    assert_eq!(summary.conflict_skipped, 1);
    // The failed task keeps its article out of AI; it is passed over, not retried.
    assert_eq!(article_state(&pool, failed).await, "persisted");
    assert_eq!(article_state(&pool, fresh).await, "ai_pending");
}

#[tokio::test]
async fn task_gen_reconciles_finished_rows_to_the_post_ai_state() {
    let (_dir, pool) = make_test_pool().await;
    let high = seed_persisted_article(&pool, "reconcile-high", "title", "body").await;
    let low = seed_persisted_article(&pool, "reconcile-low", "title", "body").await;
    let filtered = seed_persisted_article(&pool, "reconcile-filtered", "title", "body").await;
    let running = seed_persisted_article(&pool, "reconcile-running", "title", "body").await;
    seed_identity_row(&pool, high, "succeeded", Some((80, true))).await;
    seed_identity_row(&pool, low, "succeeded", Some((10, true))).await;
    seed_identity_row(&pool, filtered, "filtered", Some((0, false))).await;
    seed_identity_row(&pool, running, "running", None).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));

    let summary = flow.task_gen(&opts()).await;

    assert_eq!(summary.reconciled, 4);
    assert_eq!(summary.inserted, 0);
    // min_importance_score = 30 in opts().
    assert_eq!(article_state(&pool, high).await, "ready_for_publish");
    assert_eq!(article_state(&pool, low).await, "ai_done");
    assert_eq!(article_state(&pool, filtered).await, "publish_skipped");
    assert_eq!(article_state(&pool, running).await, "ai_pending");
}

#[tokio::test]
async fn backfilled_persisted_article_becomes_publishable_after_ai_success() {
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "backfill-e2e", "title", "body").await;
    let backfill =
        rss_ai_news_runtime::BackfillFlow::new(Arc::new(common::backfill_deps(pool.clone())));
    let backfilled = backfill
        .ai(rss_ai_news_runtime::BackfillAiOptions {
            date_from: None,
            date_to: None,
            batch_size: 1,
            new_prompt_version_tag: "backfill-e2e".to_string(),
            new_prompt_version_sha256: "sha-backfill-e2e".to_string(),
            new_prompt_version_description: "test".to_string(),
            model_id: "test-model".to_string(),
            output_schema_version: 1,
        })
        .await
        .expect("backfill should run");
    assert_eq!(backfilled.ai_tasks_inserted, 1);
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");

    let client = Arc::new(MockAiClient::default());
    let flow = flow(pool.clone(), Arc::clone(&client));
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    client.insert_success(ai_result_id, output_json(80)).await;
    let summary = flow.run(opts()).await;

    // task_gen does not add a second row: the article already left `persisted`.
    assert_eq!(summary.task_gen.scanned, 0);
    assert_eq!(summary.process.succeeded, 1);
    assert_eq!(article_state(&pool, article_id).await, "ready_for_publish");
}

// === C9: publish-window share of task_gen and claim capacity ===

fn days_ago(days: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::days(days)
}

fn hours_ago(hours: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(hours)
}

/// Sets the article's source time (`feed_entries.published_at`).
async fn set_source_time(pool: &SqlitePool, article_id: i64, at: OffsetDateTime) {
    sqlx::query(
        "UPDATE feed_entries SET published_at = ? \
         WHERE id = (SELECT origin_feed_entry_id FROM articles WHERE id = ?)",
    )
    .bind(at)
    .bind(article_id)
    .execute(pool)
    .await
    .expect("source time should update");
}

/// `count` persisted articles whose source time is `at` (minus i seconds).
async fn seed_aged(pool: &SqlitePool, tag: &str, count: usize, at: OffsetDateTime) -> Vec<i64> {
    let mut ids = Vec::new();
    for i in 0..count {
        let id = seed_persisted_article(pool, &format!("{tag}-{i}"), "title", "body").await;
        set_source_time(pool, id, at - Duration::seconds(i as i64)).await;
        ids.push(id);
    }
    ids
}

async fn with_task(pool: &SqlitePool, ids: &[i64]) -> usize {
    let mut count = 0;
    for id in ids {
        if ai_result_count_by_article(pool, *id).await > 0 {
            count += 1;
        }
    }
    count
}

#[tokio::test]
async fn fresh_article_gets_a_task_despite_a_backlog_larger_than_one_batch() {
    let (_dir, pool) = make_test_pool().await;
    let old = seed_aged(&pool, "c9-old", 6, days_ago(10)).await;
    let fresh = seed_aged(&pool, "c9-fresh", 1, hours_ago(1)).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));
    let mut opts = opts();
    opts.task_gen_batch_size = 2;

    let summary = flow.task_gen(&opts).await;

    assert_eq!(summary.inserted, 2, "the batch limit holds");
    assert_eq!(summary.fresh_advanced, 1);
    assert_eq!(article_state(&pool, fresh[0]).await, "ai_pending");
    // Older work advances in the same pass (oldest id first).
    assert_eq!(article_state(&pool, old[0]).await, "ai_pending");
    assert_eq!(with_task(&pool, &old).await, 1);
}

#[tokio::test]
async fn fresh_share_prefers_articles_that_leave_the_window_first() {
    let (_dir, pool) = make_test_pool().await;
    let late = seed_aged(&pool, "c9-late", 1, hours_ago(1)).await;
    let future = seed_aged(&pool, "c9-future", 1, hours_ago(-2)).await;
    // Highest id, but the first to leave the window.
    let early = seed_aged(&pool, "c9-early", 1, hours_ago(40)).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));
    let mut opts = opts();
    opts.task_gen_batch_size = 2; // fresh share 1, backlog share 1

    flow.task_gen(&opts).await;

    // Fresh share: the article closest to leaving the window. The backlog
    // share then takes the lowest id among the rest.
    assert_eq!(article_state(&pool, early[0]).await, "ai_pending");
    assert_eq!(article_state(&pool, late[0]).await, "ai_pending");
    assert_eq!(article_state(&pool, future[0]).await, "persisted");
}

#[tokio::test]
async fn older_backlog_keeps_advancing_while_fresh_inflow_continues() {
    let (_dir, pool) = make_test_pool().await;
    let old = seed_aged(&pool, "c9-backlog", 4, days_ago(10)).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));
    let mut opts = opts();
    opts.task_gen_batch_size = 2;
    let mut fresh = Vec::new();
    for run in 0..3 {
        // New in-window articles keep arriving, more than the fresh share.
        fresh.extend(seed_aged(&pool, &format!("c9-inflow-{run}"), 2, hours_ago(1)).await);
        flow.task_gen(&opts).await;
    }
    assert_eq!(
        with_task(&pool, &old).await,
        3,
        "one backlog article per run"
    );
    assert_eq!(
        with_task(&pool, &fresh).await,
        3,
        "one fresh article per run"
    );
}

#[tokio::test]
async fn batch_size_one_task_gen_alternates_between_fresh_and_backlog() {
    let (_dir, pool) = make_test_pool().await;
    let old = seed_aged(&pool, "c9-alt-old", 3, days_ago(10)).await;
    let fresh = seed_aged(&pool, "c9-alt-fresh", 3, hours_ago(1)).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));
    let mut opts = opts();
    opts.task_gen_batch_size = 1;
    let mut served = Vec::new();
    for _ in 0..4 {
        let summary = flow.task_gen(&opts).await;
        assert_eq!(summary.inserted, 1);
        served.push(summary.fresh_advanced == 1);
    }
    assert_eq!(served, [true, false, true, false]);
    assert_eq!(with_task(&pool, &old).await, 2);
    assert_eq!(with_task(&pool, &fresh).await, 2);
}

#[tokio::test]
async fn a_stuck_fresh_row_does_not_block_batch_size_one() {
    let (_dir, pool) = make_test_pool().await;
    let stuck = seed_aged(&pool, "c9-stuck", 1, hours_ago(40)).await;
    let next = seed_aged(&pool, "c9-next", 1, hours_ago(1)).await;
    seed_identity_row(&pool, stuck[0], "permanent_failed", None).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));
    let mut opts = opts();
    opts.task_gen_batch_size = 1;

    let first = flow.task_gen(&opts).await;
    let second = flow.task_gen(&opts).await;

    assert_eq!(first.inserted + second.inserted, 1);
    assert_eq!(article_state(&pool, stuck[0]).await, "persisted");
    assert_eq!(article_state(&pool, next[0]).await, "ai_pending");
}

#[tokio::test]
async fn window_zero_treats_every_article_as_fresh() {
    let (_dir, pool) = make_test_pool().await;
    let old = seed_aged(&pool, "c9-zero", 2, days_ago(400)).await;
    let flow = flow(pool.clone(), Arc::new(MockAiClient::default()));
    let mut opts = opts();
    opts.candidate_window_hours = 0;
    opts.task_gen_batch_size = 4;

    let summary = flow.task_gen(&opts).await;

    assert_eq!(summary.fresh_advanced, 2);
    assert_eq!(with_task(&pool, &old).await, 2);
}

/// Pending tasks for `ids`, created through task_gen so they carry the
/// identity of `opts()`; returns their AI result ids in article order.
async fn pending_tasks(pool: &SqlitePool, ids: &[i64]) -> Vec<i64> {
    let mut result_ids = Vec::new();
    for id in ids {
        seed_identity_row(pool, *id, "pending", None).await;
        sqlx::query("UPDATE articles SET state = 'ai_pending' WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        result_ids.push(ai_result_id_by_article(pool, *id).await);
    }
    result_ids
}

#[tokio::test]
async fn claim_gives_fresh_tasks_a_share_when_older_tasks_fill_the_queue() {
    let (_dir, pool) = make_test_pool().await;
    let old = seed_aged(&pool, "c9-q-old", 4, days_ago(10)).await;
    let fresh = seed_aged(&pool, "c9-q-fresh", 1, hours_ago(1)).await;
    let old_tasks = pending_tasks(&pool, &old).await;
    let fresh_task = pending_tasks(&pool, &fresh).await[0];
    let client = Arc::new(MockAiClient::default());
    for id in old_tasks.iter().chain([&fresh_task]) {
        client.insert_success(*id, output_json(80)).await;
    }
    let flow = flow(pool.clone(), Arc::clone(&client));
    let mut opts = opts();
    opts.process_batch_size = 2;
    opts.max_batches = 1;

    let summary = flow.process_ai_tasks(&opts).await;

    assert_eq!(summary.claimed, 2, "the batch limit holds");
    assert_eq!(ai_result_state(&pool, fresh_task).await, "succeeded");
    assert_eq!(ai_result_state(&pool, old_tasks[0]).await, "succeeded");
    assert_eq!(ai_result_state(&pool, old_tasks[1]).await, "pending");
}

#[tokio::test]
async fn batch_size_one_claims_alternate_between_fresh_and_older_tasks_across_runs() {
    let (_dir, pool) = make_test_pool().await;
    let old = seed_aged(&pool, "c9-c-old", 3, days_ago(10)).await;
    let fresh = seed_aged(&pool, "c9-c-fresh", 3, hours_ago(1)).await;
    let old_tasks = pending_tasks(&pool, &old).await;
    let fresh_tasks = pending_tasks(&pool, &fresh).await;
    let client = Arc::new(MockAiClient::default());
    for id in old_tasks.iter().chain(fresh_tasks.iter()) {
        client.insert_success(*id, output_json(80)).await;
    }
    let flow = flow(pool.clone(), Arc::clone(&client));
    let mut opts = opts();
    opts.process_batch_size = 1;
    opts.max_batches = 1;
    let done = |ids: Vec<i64>| {
        let pool = pool.clone();
        async move {
            let mut count = 0;
            for id in ids {
                if ai_result_state(&pool, id).await == "succeeded" {
                    count += 1;
                }
            }
            count
        }
    };
    let mut served = Vec::new();
    for _ in 0..4 {
        let before = done(fresh_tasks.clone()).await;
        let summary = flow.process_ai_tasks(&opts).await;
        assert_eq!(summary.claimed, 1);
        served.push(done(fresh_tasks.clone()).await > before);
    }
    assert_eq!(served, [true, false, true, false]);
    assert_eq!(done(old_tasks).await, 2);
}
