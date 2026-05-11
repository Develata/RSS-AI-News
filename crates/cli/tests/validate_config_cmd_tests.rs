use std::fs;
use std::path::Path;

use rss_ai_news_cli::{
    args::{Cli, Command, LogFormat, OutputFormat},
    commands::validate_config,
    error::CliError,
    exit_code::ExitCode,
};
use tempfile::TempDir;

#[tokio::test]
async fn validate_config_cmd_valid_config_returns_success() {
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), true, false);

    let cli = cli_for(temp.path());
    let summary = validate_config::run(&cli).await.expect("valid config");

    assert_eq!(summary.category_count, 1);
    assert_eq!(summary.source_count, 1);
    assert!(!summary.config_sha256.is_empty());
}

#[tokio::test]
async fn validate_config_cmd_invalid_config_returns_config_error() {
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), false, false);

    let cli = cli_for(temp.path());
    let error = validate_config::run(&cli)
        .await
        .expect_err("invalid config");

    assert!(matches!(error, CliError::Config(_)));
    assert_eq!(error.exit_code(), ExitCode::ConfigError);
}

#[tokio::test]
async fn validate_config_cmd_missing_env_with_ai_enabled_returns_config_error() {
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), true, true);

    let cli = cli_for(temp.path());
    let error = validate_config::run(&cli)
        .await
        .expect_err("missing env should fail");

    assert!(matches!(error, CliError::Config(_)));
    assert_eq!(error.exit_code(), ExitCode::ConfigError);
}

fn cli_for(config_dir: &Path) -> Cli {
    Cli {
        config_dir: config_dir.to_path_buf(),
        db_path: None,
        log_level: "info".to_string(),
        log_format: LogFormat::Pretty,
        output_format: OutputFormat::Pretty,
        dry_run: false,
        category: None,
        timezone: None,
        max_batches: None,
        command: Command::ValidateConfig,
    }
}

fn write_config(root: &Path, valid_driver: bool, ai_enabled: bool) {
    fs::create_dir_all(root.join("categories")).expect("create categories");
    let driver = if valid_driver { "sqlite" } else { "mysql" };
    let db_path = root.join("rss.sqlite").to_string_lossy().replace('\\', "/");
    let output_dir = root.join("output").to_string_lossy().replace('\\', "/");
    let artifact_dir = root.join("artifacts").to_string_lossy().replace('\\', "/");

    fs::write(
        root.join("app.toml"),
        format!(
            r#"
schema_version = "1"

[database]
driver = "{driver}"
sqlite_path = "{db_path}"
max_connections = 1
busy_timeout_ms = 5000

[http]
user_agent = "test"
timeout_seconds = 5
max_retries = 1
retry_backoff_base_ms = 1
concurrent_feeds = 1
concurrent_fetches = 1

[ai]
enabled = {ai_enabled}
model = "test-model"
max_tokens = 1024
temperature = 0.0
request_timeout_seconds = 5
max_input_chars = 1024

[ai.rate_limit]
requests_per_minute = 60
tokens_per_minute = 0

[publish]
target_timezone = "Asia/Shanghai"
github_owner = ""
github_repo = ""
github_branch = "main"
github_path_prefix = "archive"
local_output_dir = "{output_dir}"
include_unscored = false
max_items_per_report = 30
min_importance_score = 30

[dedup]
enable_link_dedup = true
enable_content_dedup = true
link_normalizer_version = "1"

[extractor]
strategy_order = ["summary_fallback"]
max_body_bytes = 1048576
min_body_chars = 1

[lease]
fetch_duration_seconds = 30
ai_duration_seconds = 30
publish_duration_seconds = 30
reclaim_interval_seconds = 30

[retry]
feed_entry_max_attempts = 1
ai_max_attempts = 1
publish_max_attempts = 1

[artifact]
retention_policy = "off"
sample_rate = 1.0
inline_threshold_bytes = 65536
file_storage_dir = "{artifact_dir}"
ttl_days = 30

[observability]
log_level = "info"
log_format = "pretty"
log_file = ""
enable_metrics = false
metrics_bind = "127.0.0.1:9090"
"#
        ),
    )
    .expect("write app");

    fs::write(
        root.join("categories").join("ai.toml"),
        r#"
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[[sources]]
key = "mock"
display_name = "Mock"
feed_url = "https://example.test/feed.xml"
feed_kind = "rss"
priority = 10
enabled = true
"#,
    )
    .expect("write category");
}
