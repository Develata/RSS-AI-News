use std::num::NonZeroU32;
use std::sync::Arc;

use rss_ai_news_config::{
    AiConfig, AiRateLimitConfig, AppConfig, ArtifactConfig, CategoryConfig, DatabaseConfig,
    DatabaseDriver, DedupConfig, DoctorConfig, EnvConfig, ExtractorConfig, HttpConfig, LeaseConfig,
    LoadedConfig, ObservabilityConfig, PublishConfig, RetentionPolicy, RetryConfig, RuntimeConfig,
};
use rss_ai_news_domain::Score0To100;
use rss_ai_news_observability::health::{
    CheckOutcome, HealthCheck, config_check::ConfigCheck, db_check::DatabaseConnectivityCheck,
    disk_check::DiskSpaceCheck, github_check::GitHubPingCheck,
    migration_check::MigrationVersionCheck, openai_check::OpenAiPingCheck,
    pending_backlog_check::PendingBacklogCheck, silent_source_check::SilentSourceCheck,
    stuck_reindex_check::StuckReindexCheck,
};
use rss_ai_news_storage::StoragePool;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn config_check_reports_ok() {
    let check = ConfigCheck::new(Arc::new(loaded_config()));
    assert!(matches!(check.run().await, CheckOutcome::Ok(_)));
}

#[tokio::test]
async fn database_check_reports_ok() {
    let pool = memory_pool().await;
    // W11-P4-C2：health check 入口接 StoragePool
    let check = DatabaseConnectivityCheck::new(StoragePool::Sqlite(pool));
    assert!(matches!(check.run().await, CheckOutcome::Ok(_)));
}

#[tokio::test]
async fn database_check_reports_fail_for_closed_pool() {
    let pool = memory_pool().await;
    pool.close().await;
    let check = DatabaseConnectivityCheck::new(StoragePool::Sqlite(pool));
    assert!(matches!(check.run().await, CheckOutcome::Fail(_)));
}

#[tokio::test]
async fn migration_check_reports_ok_when_migration_table_has_version() {
    let pool = memory_pool().await;
    sqlx::query("CREATE TABLE _sqlx_migrations (version INTEGER PRIMARY KEY)")
        .execute(&pool)
        .await
        .expect("create migrations");
    sqlx::query("INSERT INTO _sqlx_migrations (version) VALUES (1)")
        .execute(&pool)
        .await
        .expect("insert migration");

    let check = MigrationVersionCheck::new(StoragePool::Sqlite(pool));
    assert!(matches!(check.run().await, CheckOutcome::Ok(_)));
}

#[tokio::test]
async fn openai_check_reports_ok_for_chat_completion_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(&server)
        .await;

    let check = OpenAiPingCheck::new(
        reqwest::Client::new(),
        Some(server.uri()),
        Some("sk-test".to_string()),
        "gpt-test".to_string(),
        true,
    );
    assert!(matches!(check.run().await, CheckOutcome::Ok(_)));
}

#[tokio::test]
async fn openai_check_reports_fail_for_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let check = OpenAiPingCheck::new(
        reqwest::Client::new(),
        Some(server.uri()),
        Some("sk-test".to_string()),
        "gpt-test".to_string(),
        true,
    );
    assert!(matches!(check.run().await, CheckOutcome::Fail(_)));
}

#[tokio::test]
async fn github_check_reports_warn_without_token() {
    let check = GitHubPingCheck::new(reqwest::Client::new(), None);
    assert!(matches!(check.run().await, CheckOutcome::Warn(_)));
}

#[tokio::test]
async fn disk_check_reports_ok_for_tempdir() {
    let temp = TempDir::new().expect("temp dir");
    let check = DiskSpaceCheck::new(temp.path().to_path_buf(), 1);
    assert!(matches!(check.run().await, CheckOutcome::Ok(_)));
}

#[tokio::test]
async fn disk_check_reports_fail_when_minimum_is_impossible() {
    let temp = TempDir::new().expect("temp dir");
    let check = DiskSpaceCheck::new(temp.path().to_path_buf(), u64::MAX);
    assert!(matches!(check.run().await, CheckOutcome::Fail(_)));
}

// ── 3am liveness checks ────────────────────────────────────────
// 用最小表 + 固定远古/远未来时间戳（年份前缀主导 RFC3339 字典序比较），
// 与本文件既有 minimal-table 风格一致，免迁移/外键铺设。

#[tokio::test]
async fn stuck_reindex_check_warns_on_expired_running_and_stale_pending() {
    let pool = memory_pool().await;
    create_reindex_jobs_table(&pool).await;
    sqlx::query(
        "INSERT INTO reindex_jobs (state, lease_expires_at, created_at) VALUES ('running', '2000-01-01T00:00:00Z', '2000-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .expect("seed running");
    sqlx::query(
        "INSERT INTO reindex_jobs (state, lease_expires_at, created_at) VALUES ('pending', NULL, '2000-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .expect("seed pending");
    let check = StuckReindexCheck::new(StoragePool::Sqlite(pool), 3600);
    assert!(matches!(check.run().await, CheckOutcome::Warn(_)));
}

#[tokio::test]
async fn stuck_reindex_check_ok_when_lease_valid_and_pending_fresh() {
    let pool = memory_pool().await;
    create_reindex_jobs_table(&pool).await;
    sqlx::query(
        "INSERT INTO reindex_jobs (state, lease_expires_at, created_at) VALUES ('running', '2999-01-01T00:00:00Z', '2999-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .expect("seed running");
    sqlx::query(
        "INSERT INTO reindex_jobs (state, lease_expires_at, created_at) VALUES ('pending', NULL, '2999-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .expect("seed pending");
    let check = StuckReindexCheck::new(StoragePool::Sqlite(pool), 3600);
    assert!(matches!(check.run().await, CheckOutcome::Ok(_)));
}

#[tokio::test]
async fn silent_source_check_warns_on_stale_and_failing_active_sources() {
    let pool = memory_pool().await;
    create_feed_sources_table(&pool).await;
    sqlx::query(
        "INSERT INTO feed_sources (status, last_success_at, consecutive_failures) VALUES ('active', '2000-01-01T00:00:00Z', 0)",
    )
    .execute(&pool)
    .await
    .expect("seed stale");
    sqlx::query(
        "INSERT INTO feed_sources (status, last_success_at, consecutive_failures) VALUES ('active', NULL, 20)",
    )
    .execute(&pool)
    .await
    .expect("seed failing");
    let check = SilentSourceCheck::new(StoragePool::Sqlite(pool), 3600, 5);
    assert!(matches!(check.run().await, CheckOutcome::Warn(_)));
}

#[tokio::test]
async fn silent_source_check_ok_for_recent_active_and_ignores_paused() {
    let pool = memory_pool().await;
    create_feed_sources_table(&pool).await;
    // active 且最近成功 → 健康。
    sqlx::query(
        "INSERT INTO feed_sources (status, last_success_at, consecutive_failures) VALUES ('active', '2999-01-01T00:00:00Z', 0)",
    )
    .execute(&pool)
    .await
    .expect("seed recent");
    // paused 即使很久没成功 + 高失败计数也不计（仅看 active）。
    sqlx::query(
        "INSERT INTO feed_sources (status, last_success_at, consecutive_failures) VALUES ('paused', '2000-01-01T00:00:00Z', 99)",
    )
    .execute(&pool)
    .await
    .expect("seed paused");
    let check = SilentSourceCheck::new(StoragePool::Sqlite(pool), 3600, 5);
    assert!(matches!(check.run().await, CheckOutcome::Ok(_)));
}

#[tokio::test]
async fn pending_backlog_check_warns_when_ai_queue_exceeds_threshold() {
    let pool = memory_pool().await;
    create_backlog_tables(&pool).await;
    for _ in 0..3 {
        sqlx::query("INSERT INTO article_ai_results (state) VALUES ('pending')")
            .execute(&pool)
            .await
            .expect("seed pending ai");
    }
    let check = PendingBacklogCheck::new(StoragePool::Sqlite(pool), 3);
    assert!(matches!(check.run().await, CheckOutcome::Warn(_)));
}

#[tokio::test]
async fn pending_backlog_check_ok_below_threshold() {
    let pool = memory_pool().await;
    create_backlog_tables(&pool).await;
    sqlx::query("INSERT INTO feed_entries (state) VALUES ('pending_fetch')")
        .execute(&pool)
        .await
        .expect("seed fetch");
    sqlx::query("INSERT INTO article_ai_results (state) VALUES ('succeeded')")
        .execute(&pool)
        .await
        .expect("seed succeeded");
    let check = PendingBacklogCheck::new(StoragePool::Sqlite(pool), 100);
    assert!(matches!(check.run().await, CheckOutcome::Ok(_)));
}

async fn create_reindex_jobs_table(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE reindex_jobs (id INTEGER PRIMARY KEY, state TEXT NOT NULL, lease_expires_at TEXT, created_at TEXT NOT NULL)",
    )
    .execute(pool)
    .await
    .expect("create reindex_jobs");
}

async fn create_feed_sources_table(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE feed_sources (id INTEGER PRIMARY KEY, status TEXT NOT NULL, last_success_at TEXT, consecutive_failures INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(pool)
    .await
    .expect("create feed_sources");
}

async fn create_backlog_tables(pool: &SqlitePool) {
    sqlx::query("CREATE TABLE feed_entries (id INTEGER PRIMARY KEY, state TEXT NOT NULL)")
        .execute(pool)
        .await
        .expect("create feed_entries");
    sqlx::query("CREATE TABLE article_ai_results (id INTEGER PRIMARY KEY, state TEXT NOT NULL)")
        .execute(pool)
        .await
        .expect("create article_ai_results");
}

async fn memory_pool() -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("memory sqlite pool")
}

fn loaded_config() -> LoadedConfig {
    LoadedConfig {
        env: EnvConfig::default(),
        app: app_config(),
        categories: Vec::<CategoryConfig>::new(),
        source_secrets: rss_ai_news_config::SourceSecrets::default(),
        config_sha256: "sha".to_string(),
        cli_overrides: Default::default(),
    }
}

fn app_config() -> AppConfig {
    AppConfig {
        schema_version: "1".to_string(),
        database: DatabaseConfig {
            driver: DatabaseDriver::Sqlite,
            sqlite_path: "test.sqlite".into(),
            max_connections: 1,
            busy_timeout_ms: 1000,
        },
        http: HttpConfig {
            user_agent: "test".to_string(),
            timeout_seconds: 1,
            max_retries: 0,
            retry_backoff_base_ms: 1,
            concurrent_feeds: 1,
            concurrent_fetches: 1,
        },
        ai: AiConfig {
            enabled: false,
            model: "gpt-test".to_string(),
            fallback_models: Vec::new(),
            max_tokens: 1,
            temperature: 0.0,
            request_timeout_seconds: 1,
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
            template: rss_ai_news_config::PublishTemplateConfig::default(),
            include_unscored: false,
            max_items_per_report: NonZeroU32::new(30).expect("test default non-zero"),
            min_importance_score: Score0To100::try_new(30).expect("test default in range"),
            candidate_window_hours: 48,
        },
        dedup: DedupConfig {
            enable_link_dedup: true,
            enable_content_dedup: true,
            link_normalizer_version: "1".to_string(),
        },
        extractor: ExtractorConfig {
            strategy_order: vec!["summary_fallback".to_string()],
            max_body_bytes: 1024,
            feed_max_body_bytes: None,
            min_body_chars: 1,
        },
        lease: LeaseConfig {
            fetch_duration_seconds: 30,
            ai_duration_seconds: 30,
            publish_duration_seconds: 30,
            reclaim_interval_seconds: 30,
        },
        retry: RetryConfig {
            feed_entry_max_attempts: 1,
            ai_max_attempts: 1,
            publish_max_attempts: 1,
        },
        runtime: RuntimeConfig::default(),
        artifact: ArtifactConfig {
            retention_policy: RetentionPolicy::Off,
            sample_rate: 1.0,
            inline_threshold_bytes: 1024,
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
        doctor: DoctorConfig::default(),
    }
}
