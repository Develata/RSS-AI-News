#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use rss_ai_news_config::{
    AiConfig, AiRateLimitConfig, AppConfig, ArtifactConfig, CategoryConfig, CategoryMeta,
    DatabaseConfig, DatabaseDriver, DedupConfig, ExtractorConfig, HttpConfig, LeaseConfig,
    ObservabilityConfig, PublishConfig, RetentionPolicy, RetryConfig, SourceConfig,
};
use rss_ai_news_domain::state::FeedKind;
use rss_ai_news_storage::{build_sqlite_pool, run_migrations};
use sqlx::SqlitePool;

static TEST_DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub async fn make_test_pool_with_connections(max_connections: u32) -> (PathBuf, SqlitePool) {
    let counter = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "rss-ai-news-runtime-w5b-{}-{counter}",
        std::process::id()
    ));
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
