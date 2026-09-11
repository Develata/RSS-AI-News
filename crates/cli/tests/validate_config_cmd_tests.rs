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

#[tokio::test]
async fn validate_config_warns_for_inert_fields_without_rejecting_old_config() {
    let temp = TempDir::new().unwrap();
    write_config(temp.path(), true, false);
    let app_path = temp.path().join("app.toml");
    let app = fs::read_to_string(&app_path)
        .unwrap()
        .replace("enable_link_dedup = true", "enable_link_dedup = false")
        .replace("requests_per_minute = 60", "requests_per_minute = 7")
        .replace("log_level = \"info\"", "log_level = \"debug\"");
    fs::write(app_path, app).unwrap();
    let summary = validate_config::run(&cli_for(temp.path())).await.unwrap();
    let mut pretty = Vec::new();
    rss_ai_news_cli::output::CommandSummary::render_pretty(&summary, &mut pretty).unwrap();
    assert!(
        String::from_utf8(pretty)
            .unwrap()
            .contains("Warning [inert_config] dedup.enable_link_dedup")
    );
    let value = serde_json::to_value(summary).unwrap();
    let warnings = value["warnings"]
        .as_array()
        .expect("non-fatal structured warnings");
    for field in [
        "dedup.enable_link_dedup",
        "ai.rate_limit.requests_per_minute",
        "observability.log_level",
    ] {
        assert!(
            warnings.iter().any(|warning| warning["field"] == field),
            "missing {field}"
        );
    }
    assert!(!temp.path().join("rss.sqlite").exists());
}

fn cli_for(config_dir: &Path) -> Cli {
    Cli {
        config_dir: config_dir.to_path_buf(),
        db_path: None,
        log_level: "info".to_string(),
        log_format: LogFormat::Pretty,
        log_file: String::new(),
        metrics_bind: String::new(),
        output_format: OutputFormat::Pretty,
        dry_run: false,
        category: None,
        timezone: None,
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

[publish.template]
path_template = "{{CATEGORY_KEY}}/{{YYYY}}/{{YYYYMMDD}}.md"
frontmatter_template = "---\ntitle: {{date}}\ndate: {{date}}\nexcerpt: {{excerpt_yaml}}\n---\n"
report_template = "{{frontmatter}}\n# {{title_md}}\n{{excerpt_block}}\n{{items}}"
item_template = '''
## {{item_title_md}}{{score_badge}}

{{tags_block}}- **Source:** `{{source_code}}` | [阅读原文]({{url_md}})

> [摘要]  
{{summary_blockquote}}

---

'''

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

#[tokio::test]
async fn v0_7_1_config_fixture_loads_without_conversion_or_database_writes() {
    use rss_ai_news_config::{CliOverrides, load_skip_env_checks};
    use sha2::{Digest, Sha256};
    let app = include_bytes!("fixtures/v0.7.1/app.toml");
    let category = include_bytes!("fixtures/v0.7.1/ai.toml");
    assert_eq!(
        hex::encode(Sha256::digest(app)),
        "129b95583588160bef852439dba1243da8033572fc6e648f67bc3b90e89bfeca"
    );
    assert_eq!(
        hex::encode(Sha256::digest(category)),
        "685d2b68c6bca3e3344accc6abf7ba33a9b61a0fa81f31c4e155d22c2dd829a1"
    );
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("categories")).unwrap();
    fs::write(temp.path().join("app.toml"), app).unwrap();
    fs::write(temp.path().join("categories/ai.toml"), category).unwrap();
    let loaded = load_skip_env_checks(temp.path(), None, CliOverrides::default()).unwrap();
    assert_eq!(loaded.categories.len(), 1);
    assert_eq!(loaded.categories[0].category.key, "ai");
    assert_eq!(loaded.app.artifact.ttl_days, 30);
    assert!(rss_ai_news_config::validate::inert_config_warnings(&loaded.app).is_empty());
    assert_eq!(fs::read(temp.path().join("app.toml")).unwrap(), app);
    assert!(!temp.path().join("data").exists());
}
