//! `replay` through its public command entry (`replay::run`): config loading,
//! the read-only storage and the flow assembly are the real ones
//! (acceptance AC-C-01).

use std::{fs, path::Path};

use rss_ai_news_cli::{
    args::{Cli, Command, LogFormat, OutputFormat, ReplayArgs, ReplayKind},
    commands::replay,
    exit_code::ExitCode,
};
use rss_ai_news_runtime::REPLAY_HTML_NO_STRATEGY;
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use tempfile::TempDir;

/// The live extraction chain's configuration decides how an HTML replay
/// parses (R3-6): Readability enabled → the page is re-extracted; only
/// `summary_fallback` → the documented contract error (exit 1), because live
/// extraction never parses the HTML with that configuration.
#[tokio::test]
async fn html_replay_follows_the_configured_strategy_order() {
    for (strategy_order, expect_parse) in [
        (r#"["readability", "summary_fallback"]"#, true),
        (r#"["summary_fallback"]"#, false),
    ] {
        let temp = TempDir::new().expect("temp dir");
        let db_path = temp.path().join("rss.sqlite");
        write_config(temp.path(), &db_path, strategy_order);
        let artifact = seed_html_artifact(&db_path).await;

        let cli = cli_for(temp.path());
        let args = html_args(artifact);
        let result = replay::run(&cli, &args).await;
        if expect_parse {
            let summary = result.expect("readability replays the page");
            let value = serde_json::to_value(&summary).expect("json");
            assert_eq!(value["parsed"]["title"], "Replayed", "{value}");
            assert_eq!(value["parsed"]["base_uri"], "about:blank", "{value}");
        } else {
            let error = result.expect_err("summary_fallback-only has no HTML strategy");
            assert_eq!(
                error.display_user(),
                format!("config: {REPLAY_HTML_NO_STRATEGY}"),
                "{strategy_order}"
            );
            assert_eq!(error.exit_code(), ExitCode::RuntimeError);
        }
    }
}

/// R3-17: without `--id` / `--key` the usage error is reported even when the
/// database cannot be opened (it used to surface as `kind=storage`).
#[tokio::test]
async fn missing_selector_is_reported_before_the_database_is_opened() {
    let temp = TempDir::new().expect("temp dir");
    let blocker = temp.path().join("not-a-directory");
    fs::write(&blocker, b"file").expect("blocker file");
    write_config(
        temp.path(),
        &blocker.join("rss.sqlite"),
        r#"["readability"]"#,
    );
    let cli = cli_for(temp.path());
    let mut args = html_args(0);
    args.id = None;
    let error = replay::run(&cli, &args).await.expect_err("no selector");
    assert_eq!(
        error.display_user(),
        "config: replay requires either --id or --key"
    );
    assert_eq!(error.error_kind(), "runtime");
}

fn html_args(id: i64) -> ReplayArgs {
    ReplayArgs {
        kind: ReplayKind::Html,
        key: None,
        id: Some(id),
        diff: false,
    }
}

async fn seed_html_artifact(db_path: &Path) -> i64 {
    let sqlite = build_sqlite_pool(db_path, 1, 5_000).await.expect("pool");
    run_migrations(&StoragePool::Sqlite(sqlite.clone()))
        .await
        .expect("migrations");
    let paragraph =
        "Replay keeps the original bytes so the parse can be re-run offline. ".repeat(20);
    let body = format!(
        "<html><head><title>Replayed</title></head><body><article><h1>Replayed</h1>\
         <p>{paragraph}</p><p>{paragraph}</p></article></body></html>"
    );
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO raw_artifacts (kind, artifact_key, content_encoding, storage_kind, \
         inline_body, byte_size, sha256, retention_policy) VALUES ('html_payload', \
         'not-an-entry', 'identity', 'inline', ?, ?, 'sha', 'always') RETURNING id",
    )
    .bind(body.as_bytes())
    .bind(i64::try_from(body.len()).expect("size"))
    .fetch_one(&sqlite)
    .await
    .expect("artifact");
    sqlite.close().await;
    id
}

fn cli_for(config_dir: &Path) -> Cli {
    Cli {
        config_dir: config_dir.to_path_buf(),
        db_path: None,
        log_level: "info".to_string(),
        log_format: LogFormat::Pretty,
        log_file: String::new(),
        metrics_bind: String::new(),
        output_format: OutputFormat::Json,
        dry_run: false,
        category: None,
        timezone: None,
        command: Command::Replay(html_args(0)),
    }
}

/// A minimal SQLite config with the given `extractor.strategy_order`.
fn write_config(root: &Path, db_path: &Path, strategy_order: &str) {
    fs::create_dir_all(root.join("categories")).expect("create categories");
    let db_path = db_path.to_string_lossy().replace('\\', "/");
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
timeout_seconds = 1
max_retries = 0
retry_backoff_base_ms = 1
concurrent_feeds = 1
concurrent_fetches = 1

[ai]
enabled = false
model = "test-model"
max_tokens = 1024
temperature = 0.0
request_timeout_seconds = 1
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
include_unscored = true
max_items_per_report = 30
min_importance_score = 0

[publish.template]
path_template = "{{CATEGORY_KEY}}/{{YYYY}}/{{YYYYMMDD}}.md"
frontmatter_template = "---\ntitle: {{date}}\n---\n"
report_template = "{{frontmatter}}\n# {{title_md}}\n{{items}}"
item_template = "- {{item_title_md}} {{summary_blockquote}} {{url_md}}\n"

[dedup]
enable_link_dedup = true
enable_content_dedup = true
link_normalizer_version = "1"

[extractor]
strategy_order = {strategy_order}
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
publish_max_attempts = 3

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
}
