use std::num::NonZeroU32;
use std::sync::Arc;

use rss_ai_news_config::{
    AiConfig, AiRateLimitConfig, AppConfig, ArtifactConfig, CategoryConfig, DatabaseConfig,
    DatabaseDriver, DedupConfig, EnvConfig, ExtractorConfig, HttpConfig, LeaseConfig, LoadedConfig,
    ObservabilityConfig, PublishConfig, RetentionPolicy, RetryConfig, RuntimeConfig,
};
use rss_ai_news_domain::Score0To100;
use rss_ai_news_observability::health::{
    CheckOutcome, HealthCheck, config_check::ConfigCheck, db_check::DatabaseConnectivityCheck,
    disk_check::DiskSpaceCheck, github_check::GitHubPingCheck,
    migration_check::MigrationVersionCheck, openai_check::OpenAiPingCheck,
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
        },
        dedup: DedupConfig {
            enable_link_dedup: true,
            enable_content_dedup: true,
            link_normalizer_version: "1".to_string(),
        },
        extractor: ExtractorConfig {
            strategy_order: vec!["summary_fallback".to_string()],
            max_body_bytes: 1024,
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
    }
}
