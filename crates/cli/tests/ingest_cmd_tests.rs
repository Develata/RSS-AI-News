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
async fn readability_is_active_without_ai_or_publish_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(rss_body(&server.uri())))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/article-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(include_str!(
            "../../extractor/tests/fixtures/simple_article.html"
        )))
        .mount(&server)
        .await;
    let temp = TempDir::new().unwrap();
    write_config(temp.path(), &server.uri());
    let app_path = temp.path().join("app.toml");
    let app = fs::read_to_string(&app_path)
        .unwrap()
        .replace(
            "strategy_order = [\"summary_fallback\"]",
            "strategy_order = [\"readability\", \"summary_fallback\"]",
        )
        .replace("enabled = false", "enabled = true");
    fs::write(app_path, app).unwrap();
    let args = IngestArgs::default();
    let summary = ingest::run(&cli_for(temp.path(), args.clone()), &args)
        .await
        .unwrap();
    assert_eq!(summary.articles_persisted, 1);
    assert_eq!(summary.articles_fallback, 0);
}

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

/// A11: an extract batch that could outlive its lease no longer refuses to
/// start (configs that ran before the budget existed keep running); the batch
/// is shrunk to whole waves instead (arithmetic pinned in the runtime unit test
/// `over_budget_batches_shrink_to_whole_waves_that_fit_the_lease`).
#[tokio::test]
async fn extract_batch_over_the_lease_is_shrunk_not_rejected() {
    let server = MockServer::start().await;
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), &server.uri());
    let app_path = temp.path().join("app.toml");
    // batch 50, 1 concurrent fetch, 5 s timeout → 50 × (5 + 5) = 500 s > 400 s.
    let app = fs::read_to_string(&app_path).unwrap().replace(
        "fetch_duration_seconds = 600",
        "fetch_duration_seconds = 400",
    );
    fs::write(&app_path, app).unwrap();

    // `IngestArgs::default()` has batch 0; the CLI default is 50.
    let args = IngestArgs {
        batch_size: 50,
        ..IngestArgs::default()
    };
    ingest::run(&cli_for(temp.path(), args.clone()), &args)
        .await
        .expect("an over-budget batch is shrunk, not rejected");

    let skip = IngestArgs {
        skip_fetch: true,
        ..args.clone()
    };
    ingest::run(&cli_for(temp.path(), skip.clone()), &skip)
        .await
        .expect("--skip-fetch runs no extract");
}

/// Five feed items with distinct summaries (no content dedup).
fn rss_body_with_items(server_uri: &str, count: usize) -> String {
    let items: String = (1..=count)
        .map(|i| {
            format!(
                "<item><guid>item-{i}</guid><title>Item {i}</title>\
                 <link>{server_uri}/article-{i}</link>\
                 <description>Distinct summary number {i} for fallback.</description>\
                 <pubDate>Thu, 30 Apr 2026 10:00:00 GMT</pubDate></item>"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?><rss version=\"2.0\"><channel>\
         <title>Mock Feed</title><link>{server_uri}</link><description>d</description>\
         {items}</channel></rss>"
    )
}

async fn mount_feed_with_items(server: &MockServer, count: usize) {
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(rss_body_with_items(&server.uri(), count)),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(r"^/article-\d+$"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html><body>x</body></html>"))
        .mount(server)
        .await;
}

fn set_lease(root: &Path, seconds: u64) {
    let app_path = root.join("app.toml");
    let app = fs::read_to_string(&app_path).unwrap().replace(
        "fetch_duration_seconds = 600",
        &format!("fetch_duration_seconds = {seconds}"),
    );
    fs::write(&app_path, app).unwrap();
}

/// R2-S9: the shrunk batch is the one the run claims. 1 concurrent fetch,
/// 5 s timeout → 10 s per wave; a 30 s lease fits 3 waves, so with one batch
/// per run exactly 3 of the 5 entries are extracted.
#[tokio::test]
async fn the_shrunk_extract_batch_is_the_one_claimed() {
    let server = MockServer::start().await;
    mount_feed_with_items(&server, 5).await;
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), &server.uri());
    set_lease(temp.path(), 30);
    let args = IngestArgs {
        batch_size: 50,
        max_batches: Some(1),
        ..IngestArgs::default()
    };

    let summary = ingest::run(&cli_for(temp.path(), args.clone()), &args)
        .await
        .unwrap();

    assert_eq!(summary.entries_inserted, 5);
    assert_eq!(
        summary.articles_persisted + summary.articles_fallback + summary.fetch_failed,
        3
    );
    assert_eq!(
        summary.exit_code(),
        rss_ai_news_cli::exit_code::ExitCode::Success
    );
}

/// R2-S9: when not even one wave fits the lease, ingest still runs but
/// extract claims nothing this run, and the run reports it (exit 1).
#[tokio::test]
async fn extract_is_skipped_with_an_error_when_no_wave_fits_the_lease() {
    let server = MockServer::start().await;
    mount_feed_with_items(&server, 2).await;
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), &server.uri());
    // 10 s per wave > 8 s lease.
    set_lease(temp.path(), 8);
    let args = IngestArgs {
        batch_size: 50,
        ..IngestArgs::default()
    };

    let summary = ingest::run(&cli_for(temp.path(), args.clone()), &args)
        .await
        .unwrap();

    assert_eq!(summary.entries_inserted, 2, "ingest still ran");
    assert_eq!(
        summary.articles_persisted + summary.articles_fallback + summary.fetch_failed,
        0,
        "nothing was claimed"
    );
    assert_eq!(
        summary.exit_code(),
        rss_ai_news_cli::exit_code::ExitCode::RuntimeError
    );
    assert!(
        summary
            .errors()
            .iter()
            .any(|error| error.kind == "extract_lease_too_short")
    );
}

fn cli_for(config_dir: &Path, args: IngestArgs) -> Cli {
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
fetch_duration_seconds = 600
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
