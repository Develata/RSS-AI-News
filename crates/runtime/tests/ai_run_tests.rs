mod common;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_ai::{AiClient, AiError, AiResponse, AiTask, TokenUsage};
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_feed::fetcher::RawFeedFetch;
use rss_ai_news_feed::{FeedError, FeedFetcher};
use rss_ai_news_publish::LocalFsTarget;
use rss_ai_news_runtime::{AiRunFlow, AiRunOptions, RunContext, RunContextDeps};
use rss_ai_news_storage::{
    ArticleAiResultRepo, ArticleRepo, FeedEntryRepo, FeedSourceRepo, PublishItemRepo,
    PublishRecordRepo, RawArtifactRepo, RunEventRepo,
};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use common::{DummyHtmlFetcher, app_config, make_test_pool, seed_persisted_article};

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
async fn process_aborts_fallback_when_lease_expired() {
    // codex P2：lease 到期后不再继续 fallback。把 ai_duration_seconds 设为 0 →
    // 本批 lease 截止时刻 = claim 时刻；首个模型尝试返回后 now() 已 ≥ 截止，
    // fallback 前的到期校验应中止链，绝不消费 fallback 响应。该时序仅在 CLI
    // 预算被绕过（如此处直接驱动 AiRunFlow）时可达。
    let (_dir, pool) = make_test_pool().await;
    let article_id = seed_persisted_article(&pool, "ai-fallback-lease", "title", "body").await;
    let client = Arc::new(MockAiClient::default());
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.lease.ai_duration_seconds = 0;
    let flow = flow_with_app(pool.clone(), Arc::clone(&client), app);
    let mut opts = opts();
    opts.fallback_models = vec!["fallback-model".to_string()];
    flow.task_gen(&opts).await;
    let ai_result_id = ai_result_id_by_article(&pool, article_id).await;
    // primary 命中 quota（should_fallback=true，但 is_retryable=false → permanent）。
    client
        .insert_error(
            ai_result_id,
            AiError::QuotaExceeded {
                message: "no quota".to_string(),
            },
        )
        .await;
    // 这条成功响应不应被消费（fallback 因 lease 到期被中止）。
    client.insert_success(ai_result_id, output_json(80)).await;

    let summary = flow.process_ai_tasks(&opts).await;

    assert_eq!(summary.succeeded, 0);
    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(
        ai_result_state(&pool, ai_result_id).await,
        "permanent_failed"
    );
    // fallback 模型从未被调用：成功响应仍原样留在队列里。
    assert_eq!(client.remaining(ai_result_id).await, 1);
    // effective_model_id 未被写成 fallback-model（成功路径未走）。
    let effective: Option<String> =
        sqlx::query_scalar("SELECT effective_model_id FROM article_ai_results WHERE id = ?")
            .bind(ai_result_id)
            .fetch_one(&pool)
            .await
            .expect("ai result row readable");
    assert_ne!(effective.as_deref(), Some("fallback-model"));
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

fn flow(pool: SqlitePool, ai_client: Arc<MockAiClient>) -> AiRunFlow {
    flow_with_app(pool, ai_client, app_config(RetentionPolicy::Always, 1))
}

fn flow_with_app(
    pool: SqlitePool,
    ai_client: Arc<MockAiClient>,
    app: rss_ai_news_config::AppConfig,
) -> AiRunFlow {
    let app = Arc::new(app);
    let ctx = Arc::new(RunContext::new_for_stage(
        "ai_run",
        app,
        RunContextDeps {
            feed_fetcher: Arc::new(DummyFeedFetcher),
            html_fetcher: Arc::new(DummyHtmlFetcher),
            strategies: Vec::new(),
            ai_client,
            publish_target_local: Arc::new(LocalFsTarget::new(std::env::temp_dir())),
            publish_target_remote: None,
            feed_source_repo: Arc::new(FeedSourceRepo::new(pool.clone())),
            feed_entry_repo: Arc::new(FeedEntryRepo::new(pool.clone())),
            article_repo: Arc::new(ArticleRepo::new(pool.clone())),
            ai_result_repo: Arc::new(ArticleAiResultRepo::new(pool.clone())),
            publish_record_repo: Arc::new(PublishRecordRepo::new(pool.clone())),
            publish_item_repo: Arc::new(PublishItemRepo::new(pool.clone())),
            artifact_repo: Arc::new(RawArtifactRepo::new(pool.clone())),
            event_repo: Arc::new(RunEventRepo::new(pool.clone())),
            rule_version_repo: Arc::new(rss_ai_news_storage::RuleVersionRepo::new(pool.clone())),
            reindex_job_repo: Arc::new(rss_ai_news_storage::ReindexJobRepo::new(pool)),
        },
    ));
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
        prompt_template: "Title: {title}\nCategory: {category_key}\nBody: {body_text}".to_string(),
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

    /// 队列里尚未被 `invoke` 消费的响应条数。用于断言某次模型尝试是否真的发起
    /// （fallback 被中止时，后续响应应原样留在队列里）。
    async fn remaining(&self, ai_result_id: i64) -> usize {
        self.responses
            .lock()
            .await
            .get(&ai_result_id)
            .map_or(0, VecDeque::len)
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

struct DummyFeedFetcher;

#[async_trait]
impl FeedFetcher for DummyFeedFetcher {
    async fn fetch_raw(&self, _req: &FeedFetchRequest) -> Result<RawFeedFetch, FeedError> {
        Err(FeedError::ConnectionFailed {
            source: "dummy".to_string(),
        })
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
