use std::fs;
use std::path::Path;

use rss_ai_news_cli::{
    args::{Cli, Command, IngestArgs, LogFormat, OutputFormat},
    commands::ingest,
    output::CommandSummary,
};
use serde_json::json;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn ingest_cmd_with_mock_feed_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(rss_body(&server.uri())))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/article-1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                "<html><body><article><p>Article body</p></article></body></html>",
            ),
        )
        .mount(&server)
        .await;

    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), &server.uri());
    let cli = cli_for(temp.path(), IngestArgs::default());

    let summary = ingest::run(
        &cli,
        match &cli.command {
            Command::Ingest(args) => args,
            _ => panic!("expected ingest command"),
        },
    )
    .await
    .expect("ingest succeeds");

    assert_eq!(summary.sources_attempted, 1);
    assert_eq!(summary.sources_succeeded, 1);
    assert!(summary.entries_inserted >= 1);
    assert!(summary.articles_fallback >= 1);

    let value = serde_json::to_value(&summary).expect("serialize");
    let envelope = json!({
        "command": "ingest",
        "status": "success",
        "summary": value,
        "errors": [],
    });
    assert_eq!(envelope["command"], "ingest");
    assert_eq!(envelope["status"], "success");

    let mut pretty = Vec::new();
    summary.render_pretty(&mut pretty).expect("pretty render");
    let pretty = String::from_utf8(pretty).expect("utf8");
    assert!(pretty.contains("Ingest completed:"));
    assert!(pretty.contains("Sources succeeded:    1"));
}

#[tokio::test]
async fn ingest_cmd_with_failing_feed_records_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), &server.uri());
    let cli = cli_for(
        temp.path(),
        IngestArgs {
            skip_fetch: true,
            ..IngestArgs::default()
        },
    );

    let summary = ingest::run(
        &cli,
        match &cli.command {
            Command::Ingest(args) => args,
            _ => panic!("expected ingest command"),
        },
    )
    .await
    .expect("partial feed failure is still command success");

    assert_eq!(summary.sources_attempted, 1);
    assert_eq!(summary.sources_failed, 1);
    assert_eq!(summary.entries_inserted, 0);
}

fn cli_for(config_dir: &Path, args: IngestArgs) -> Cli {
    Cli {
        config_dir: config_dir.to_path_buf(),
        db_path: None,
        log_level: "info".to_string(),
        log_format: LogFormat::Pretty,
        log_file: String::new(),
        output_format: OutputFormat::Pretty,
        dry_run: false,
        category: None,
        timezone: None,
        command: Command::Ingest(args),
    }
}

fn write_config(root: &Path, server_uri: &str) {
    fs::create_dir_all(root.join("categories")).expect("create categories");
    let db_path = root.join("rss.sqlite").to_string_lossy().replace('\\', "/");
    let output_dir = root.join("output").to_string_lossy().replace('\\', "/");
    let artifact_dir = root.join("artifacts").to_string_lossy().replace('\\', "/");

    fs::write(
        root.join("app.toml"),
        format!(
            r#"
schema_version = "1"

[database]
driver = "sqlite"
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
enabled = false
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
        format!(
            r#"
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[[sources]]
key = "mock"
display_name = "Mock"
feed_url = "{server_uri}/feed.xml"
feed_kind = "rss"
priority = 10
enabled = true
"#
        ),
    )
    .expect("write category");
}

fn rss_body(server_uri: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<rss version="2.0">
  <channel>
    <title>Mock Feed</title>
    <link>{server_uri}</link>
    <description>Mock feed</description>
    <item>
      <guid>item-1</guid>
      <title>Item 1</title>
      <link>{server_uri}/article-1</link>
      <description>Short summary for fallback extraction.</description>
      <pubDate>Thu, 30 Apr 2026 10:00:00 GMT</pubDate>
    </item>
  </channel>
</rss>"#
    )
}
