#![allow(dead_code)]

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rss_ai_news_ai::{AiClient, AiError, AiResponse, AiTask};
use rss_ai_news_config::{
    AiConfig, AiRateLimitConfig, AppConfig, ArtifactConfig, CategoryConfig, CategoryMeta,
    DatabaseConfig, DatabaseDriver, DedupConfig, ExtractorConfig, HttpConfig, LeaseConfig,
    ObservabilityConfig, PublishConfig, RetentionPolicy, RetryConfig, RuntimeConfig, SourceConfig,
};
use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::extract::ArticleFetchTask;
use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_domain::state::FeedKind;
use rss_ai_news_extractor::{ExtractorError, HtmlFetcher, RawHtmlFetch};
use rss_ai_news_feed::FeedFetcher;
use rss_ai_news_publish::{LocalFsTarget, PublishError, PublishTarget, PublishedArtifact};
use rss_ai_news_runtime::{RunContext, RunContextDeps};
use rss_ai_news_storage::{
    SqliteArticleAiResultRepo, SqliteArticleRepo, SqliteFeedEntryRepo, SqliteFeedSourceRepo,
    SqlitePublishItemRepo, SqlitePublishRecordRepo, SqliteRawArtifactRepo, SqliteRunEventRepo,
    build_sqlite_pool, run_migrations,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

static TEST_DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// F8-3 W4-3：在 PID + atomic counter 之上再叠纳秒时间戳，避免 OS PID 复用
/// + 残留 SQLite 文件导致跨进程偶发 UNIQUE 冲突。三层叠加（pid / nanos /
/// counter）任何一层独立就足以唯一；并发同 100ns 窗口的概率近 0。
fn unique_path_suffix() -> String {
    let counter = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{counter}", std::process::id())
}

pub async fn make_test_pool_with_connections(max_connections: u32) -> (PathBuf, SqlitePool) {
    let dir =
        std::env::temp_dir().join(format!("rss-ai-news-runtime-w5b-{}", unique_path_suffix()));
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    let db_path = dir.join("test.sqlite");
    let pool = build_sqlite_pool(&db_path, max_connections, 5_000)
        .await
        .expect("test pool should be created");
    run_migrations(&pool)
        .await
        .expect("migrations should apply");
    (dir, pool)
}

pub async fn make_test_pool() -> (PathBuf, SqlitePool) {
    make_test_pool_with_connections(4).await
}

pub async fn insert_config_rule(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) VALUES ('config', ?, 'test config', ?) RETURNING id",
    )
    .bind(format!("cfg-{}", TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)))
    .bind(format!("sha-{}", TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)))
    .fetch_one(pool)
    .await
    .expect("config rule should insert")
}

pub async fn insert_source(
    pool: &SqlitePool,
    config_version: i64,
    source_key: &str,
    feed_url: &str,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind, config_version
        )
        VALUES ('ai', ?, ?, ?, 'rss', ?)
        RETURNING id
        "#,
    )
    .bind(source_key)
    .bind(format!("Source {source_key}"))
    .bind(feed_url)
    .bind(config_version)
    .fetch_one(pool)
    .await
    .expect("feed source should insert")
}

pub fn app_config(retention_policy: RetentionPolicy, concurrent_feeds: u32) -> AppConfig {
    AppConfig {
        schema_version: "1".to_string(),
        database: DatabaseConfig {
            driver: DatabaseDriver::Sqlite,
            sqlite_path: "test.sqlite".into(),
            max_connections: 4,
            busy_timeout_ms: 5_000,
        },
        http: HttpConfig {
            user_agent: "test".to_string(),
            timeout_seconds: 5,
            max_retries: 0,
            retry_backoff_base_ms: 0,
            concurrent_feeds,
            concurrent_fetches: 2,
        },
        ai: AiConfig {
            enabled: true,
            model: "test-model".to_string(),
            max_tokens: 1024,
            temperature: 0.0,
            request_timeout_seconds: 5,
            max_input_chars: 1024,
            rate_limit: AiRateLimitConfig {
                requests_per_minute: 60,
                tokens_per_minute: 0,
            },
        },
        publish: PublishConfig {
            target_timezone: "Asia/Shanghai".to_string(),
            github_owner: String::new(),
            github_repo: String::new(),
            github_branch: "main".to_string(),
            github_path_prefix: "archive".to_string(),
            local_output_dir: "output".into(),
            include_unscored: false,
            max_items_per_report: NonZeroU32::new(30).expect("test default non-zero"),
            min_importance_score: Score0To100::try_new(30).expect("test default in range"),
        },
        dedup: DedupConfig {
            enable_link_dedup: true,
            enable_content_dedup: true,
            link_normalizer_version: "1".to_string(),
        },
        extractor: ExtractorConfig {
            strategy_order: vec!["readability".to_string()],
            max_body_bytes: 1024 * 1024,
            min_body_chars: 1,
        },
        lease: LeaseConfig {
            fetch_duration_seconds: 30,
            ai_duration_seconds: 30,
            publish_duration_seconds: 30,
            reclaim_interval_seconds: 30,
        },
        retry: RetryConfig {
            feed_entry_max_attempts: 5,
            ai_max_attempts: 3,
            publish_max_attempts: 5,
        },
        runtime: RuntimeConfig::default(),
        artifact: ArtifactConfig {
            retention_policy,
            sample_rate: 1.0,
            inline_threshold_bytes: 64 * 1024,
            file_storage_dir: "artifacts".into(),
            ttl_days: 30,
        },
        observability: ObservabilityConfig {
            log_level: "info".to_string(),
            log_format: "pretty".to_string(),
            log_file: String::new(),
            enable_metrics: false,
            metrics_bind: "127.0.0.1:9090".to_string(),
        },
    }
}

pub fn full_context(
    stage: &str,
    pool: SqlitePool,
    app: Arc<AppConfig>,
    feed_fetcher: Arc<dyn FeedFetcher>,
) -> RunContext {
    let dir = std::env::temp_dir().join(format!(
        "rss-ai-news-runtime-publish-output-{}",
        unique_path_suffix()
    ));
    std::fs::create_dir_all(&dir).expect("publish output dir should be created");
    full_context_with_publish_target(
        stage,
        pool,
        app,
        feed_fetcher,
        Arc::new(LocalFsTarget::new(dir)),
    )
}

pub fn full_context_with_publish_target(
    stage: &str,
    pool: SqlitePool,
    app: Arc<AppConfig>,
    feed_fetcher: Arc<dyn FeedFetcher>,
    publish_target_local: Arc<dyn PublishTarget>,
) -> RunContext {
    full_context_with_publish_targets(stage, pool, app, feed_fetcher, publish_target_local, None)
}

pub fn full_context_with_publish_targets(
    stage: &str,
    pool: SqlitePool,
    app: Arc<AppConfig>,
    feed_fetcher: Arc<dyn FeedFetcher>,
    publish_target_local: Arc<dyn PublishTarget>,
    publish_target_remote: Option<Arc<dyn PublishTarget>>,
) -> RunContext {
    RunContext::new_for_stage(
        stage,
        app,
        RunContextDeps {
            feed_fetcher,
            html_fetcher: Arc::new(DummyHtmlFetcher),
            strategies: Vec::new(),
            ai_client: Arc::new(DummyAiClient),
            publish_target_local,
            publish_target_remote,
            feed_source_repo: Arc::new(SqliteFeedSourceRepo::new(pool.clone())),
            feed_entry_repo: Arc::new(SqliteFeedEntryRepo::new(pool.clone())),
            article_repo: Arc::new(SqliteArticleRepo::new(pool.clone())),
            ai_result_repo: Arc::new(SqliteArticleAiResultRepo::new(pool.clone())),
            publish_record_repo: Arc::new(SqlitePublishRecordRepo::new(pool.clone())),
            publish_item_repo: Arc::new(SqlitePublishItemRepo::new(pool.clone())),
            artifact_repo: Arc::new(SqliteRawArtifactRepo::new(pool.clone())),
            event_repo: Arc::new(SqliteRunEventRepo::new(pool.clone())),
            rule_version_repo: Arc::new(rss_ai_news_storage::SqliteRuleVersionRepo::new(pool)),
        },
    )
}

pub async fn seed_snapshot_frozen_publish_record(
    pool: &SqlitePool,
    remote_target: Option<String>,
) -> i64 {
    let render = insert_config_rule(pool).await;
    let policy = insert_config_rule(pool).await;
    let id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO publish_records (
            idempotency_key, category_key, report_date, target_timezone,
            render_version, selection_policy_version, state, snapshot_frozen_at, remote_target
        )
        VALUES (?, 'ai', '2026-04-28', 'Asia/Shanghai', ?, ?, 'snapshot_frozen', ?, ?)
        RETURNING id
        "#,
    )
    .bind(format!(
        "ai-2026-04-28-v{}",
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
    .bind(render)
    .bind(policy)
    .bind(OffsetDateTime::now_utc())
    .bind(remote_target)
    .fetch_one(pool)
    .await
    .expect("snapshot frozen record should insert");

    let (article_id, ai_result_id) = seed_ai_succeeded_article(
        pool,
        "ai",
        &format!(
            "snapshot-{}",
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        ),
        "Title",
        "body",
        "summary",
        88,
        1,
    )
    .await;
    sqlx::query(
        r#"
        INSERT INTO publish_items (
            publish_record_id, position, article_id, article_ai_result_id,
            frozen_title, frozen_summary, frozen_tags_json, frozen_score,
            frozen_canonical_link, frozen_source_display_name
        )
        VALUES (?, 1, ?, ?, 'Title', 'summary', '["ai"]', 88, 'https://example.com/article', 'Source')
        "#,
    )
    .bind(id)
    .bind(article_id)
    .bind(ai_result_id)
    .execute(pool)
    .await
    .expect("publish item should insert");
    id
}

pub async fn seed_rendered_publish_record(
    pool: &SqlitePool,
    remote_target: Option<String>,
    rendered_at: OffsetDateTime,
) -> i64 {
    let id = seed_snapshot_frozen_publish_record(pool, remote_target).await;
    sqlx::query("UPDATE publish_records SET state = 'rendered', rendered_at = ? WHERE id = ?")
        .bind(rendered_at)
        .bind(id)
        .execute(pool)
        .await
        .expect("record should advance to rendered");
    id
}

pub async fn seed_pending_fetch_entry(
    pool: &SqlitePool,
    source_id: i64,
    uid: &str,
    link_hash: &str,
    summary_raw: Option<&str>,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            summary_raw, discovered_at, state, dedup_decision
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, 'pending_fetch', 'fresh')
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(uid)
    .bind(format!("https://example.com/{uid}"))
    .bind(link_hash)
    .bind(format!("title {uid}"))
    .bind(summary_raw)
    .bind(OffsetDateTime::now_utc())
    .fetch_one(pool)
    .await
    .expect("pending fetch entry should insert")
}

pub async fn seed_extractor_rule_version(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) VALUES ('extractor', ?, 'test extractor', ?) RETURNING id",
    )
    .bind(format!(
        "extractor-{}",
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
    .bind(format!(
        "extractor-sha-{}",
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
    .fetch_one(pool)
    .await
    .expect("extractor rule should insert")
}

pub async fn seed_persisted_article(
    pool: &SqlitePool,
    content_hash: &str,
    title: &str,
    body_text: &str,
) -> i64 {
    let extractor_version = seed_extractor_rule_version(pool).await;
    let config_version = insert_config_rule(pool).await;
    let source_id = insert_source(
        pool,
        config_version,
        &format!("source-{content_hash}"),
        &format!("https://example.com/{content_hash}.xml"),
    )
    .await;
    let entry_id = seed_pending_fetch_entry(
        pool,
        source_id,
        &format!("uid-{content_hash}"),
        &format!("link-hash-{content_hash}"),
        None,
    )
    .await;
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO articles (
            content_hash, canonical_link, title, body_text, extractor_strategy,
            extractor_version, content_quality, word_count, origin_feed_entry_id, state
        )
        VALUES (?, ?, ?, ?, 'readability', ?, 'high', ?, ?, 'persisted')
        RETURNING id
        "#,
    )
    .bind(content_hash)
    .bind(format!("https://example.com/article/{content_hash}"))
    .bind(title)
    .bind(body_text)
    .bind(extractor_version)
    .bind(body_text.split_whitespace().count() as i64)
    .bind(entry_id)
    .fetch_one(pool)
    .await
    .expect("persisted article should insert")
}

pub async fn seed_ai_succeeded_article(
    pool: &SqlitePool,
    category_key: &str,
    content_hash: &str,
    title: &str,
    body_text: &str,
    summary: &str,
    importance_score: i32,
    keep_decision: i32,
) -> (i64, i64) {
    let extractor_version = seed_extractor_rule_version(pool).await;
    let prompt_version = insert_config_rule(pool).await;
    let output_schema_version = seed_output_schema_rule_version(pool).await;
    let config_version = insert_config_rule(pool).await;
    let source_id = insert_source_with_category(
        pool,
        config_version,
        category_key,
        &format!("source-{content_hash}"),
        &format!("https://example.com/{content_hash}.xml"),
    )
    .await;
    let entry_id = seed_pending_fetch_entry(
        pool,
        source_id,
        &format!("uid-{content_hash}"),
        &format!("link-hash-{content_hash}"),
        Some("raw summary"),
    )
    .await;
    let article_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO articles (
            content_hash, canonical_link, title, body_text, extractor_strategy,
            extractor_version, content_quality, word_count, origin_feed_entry_id, state
        )
        VALUES (?, ?, ?, ?, 'readability', ?, 'high', ?, ?, 'ready_for_publish')
        RETURNING id
        "#,
    )
    .bind(content_hash)
    .bind(format!("https://example.com/article/{content_hash}"))
    .bind(title)
    .bind(body_text)
    .bind(extractor_version)
    .bind(body_text.split_whitespace().count() as i64)
    .bind(entry_id)
    .fetch_one(pool)
    .await
    .expect("ready article should insert");
    let ai_result_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO article_ai_results (
            article_id, prompt_version, output_schema_version, model_id, state,
            summary, tags_json, importance_score, keep_decision, completed_at
        )
        VALUES (?, ?, ?, 'test-model', 'succeeded', ?, '["ai"]', ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(article_id)
    .bind(prompt_version)
    .bind(output_schema_version)
    .bind(summary)
    .bind(importance_score)
    .bind(keep_decision)
    .bind(OffsetDateTime::now_utc())
    .fetch_one(pool)
    .await
    .expect("AI result should insert");
    (article_id, ai_result_id)
}

pub async fn seed_persisted_article_for_passthrough(
    pool: &SqlitePool,
    category_key: &str,
    content_hash: &str,
    title: &str,
    summary_raw: &str,
) -> i64 {
    let extractor_version = seed_extractor_rule_version(pool).await;
    let config_version = insert_config_rule(pool).await;
    let source_id = insert_source_with_category(
        pool,
        config_version,
        category_key,
        &format!("source-{content_hash}"),
        &format!("https://example.com/{content_hash}.xml"),
    )
    .await;
    let entry_id = seed_pending_fetch_entry(
        pool,
        source_id,
        &format!("uid-{content_hash}"),
        &format!("link-hash-{content_hash}"),
        Some(summary_raw),
    )
    .await;
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO articles (
            content_hash, canonical_link, title, body_text, extractor_strategy,
            extractor_version, content_quality, word_count, origin_feed_entry_id, state
        )
        VALUES (?, ?, ?, ?, 'readability', ?, 'high', 1, ?, 'persisted')
        RETURNING id
        "#,
    )
    .bind(content_hash)
    .bind(format!("https://example.com/article/{content_hash}"))
    .bind(title)
    .bind(summary_raw)
    .bind(extractor_version)
    .bind(entry_id)
    .fetch_one(pool)
    .await
    .expect("passthrough article should insert")
}

pub async fn seed_output_schema_rule_version(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) VALUES ('ai_output_schema', ?, 'test output schema', ?) RETURNING id",
    )
    .bind(format!(
        "schema-{}",
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
    .bind(format!(
        "schema-sha-{}",
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
    .fetch_one(pool)
    .await
    .expect("output schema rule should insert")
}

pub async fn insert_source_with_category(
    pool: &SqlitePool,
    config_version: i64,
    category_key: &str,
    source_key: &str,
    feed_url: &str,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind, config_version
        )
        VALUES (?, ?, ?, ?, 'rss', ?)
        RETURNING id
        "#,
    )
    .bind(category_key)
    .bind(source_key)
    .bind(format!("Source {source_key}"))
    .bind(feed_url)
    .bind(config_version)
    .fetch_one(pool)
    .await
    .expect("feed source should insert")
}

pub fn category_with_sources(source_keys: &[&str]) -> CategoryConfig {
    CategoryConfig {
        schema_version: "1".to_string(),
        category: CategoryMeta {
            key: "ai".to_string(),
            display_name: "AI".to_string(),
            priority: 10,
        },
        ai_override: None,
        publish_override: None,
        sources: source_keys
            .iter()
            .map(|key| SourceConfig {
                key: (*key).to_string(),
                display_name: format!("Source {key}"),
                feed_url: format!("https://example.com/{key}.xml"),
                feed_kind: FeedKind::Rss,
                priority: 10,
                enabled: true,
            })
            .collect(),
    }
}

pub struct DummyHtmlFetcher;

#[async_trait]
impl HtmlFetcher for DummyHtmlFetcher {
    async fn fetch_html(&self, _task: &ArticleFetchTask) -> Result<RawHtmlFetch, ExtractorError> {
        Err(ExtractorError::ConnectionFailed)
    }
}

pub struct DummyAiClient;

#[async_trait]
impl AiClient for DummyAiClient {
    async fn invoke(&self, _task: &AiTask) -> Result<AiResponse, AiError> {
        Err(AiError::ConnectionFailed("dummy".to_string()))
    }
}

pub struct MockFailingTarget;

#[async_trait]
impl PublishTarget for MockFailingTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        Err(PublishError::LocalIoError(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "mock local write denied",
        )))
    }
}

/// 第一次 `publish` 调用返回 retryable `LocalIoError(TimedOut)`，之后委托
/// 给 inner target 正常落盘。用于验证 release_retryable_failure 不改
/// `publish_records.state`，下次 claim 可重新捞回并成功。
pub struct MockOnceRetryableThenInner {
    attempts: AtomicUsize,
    inner: Arc<dyn PublishTarget>,
}

impl MockOnceRetryableThenInner {
    pub fn new(inner: Arc<dyn PublishTarget>) -> Self {
        Self {
            attempts: AtomicUsize::new(0),
            inner,
        }
    }
}

#[async_trait]
impl PublishTarget for MockOnceRetryableThenInner {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        let n = self.attempts.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(PublishError::LocalIoError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "mock first attempt timeout",
            )));
        }
        self.inner.publish(report).await
    }
}
