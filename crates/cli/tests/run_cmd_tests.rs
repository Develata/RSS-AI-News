//! `run` command entry (`run::run`, which loads the config itself): stage
//! isolation counterexamples (docs/plan/11 §5, D12). Each test re-executes
//! itself in a child test process with a controlled environment, because the
//! config loader reads the process environment (and `./.env`) and expands
//! RSSHub placeholders at load time — clearing `LoadedConfig::env` afterwards
//! cannot undo that.

use std::{
    fs,
    path::Path,
    process::{Command as Process, Stdio},
};

use rss_ai_news_cli::{
    args::{Cli, Command, LogFormat, OutputFormat, RunArgs},
    commands::run,
    exit_code::ExitCode,
    output::CommandSummary,
};
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};

const CHILD_MARKER: &str = "RSS_AI_NEWS_RUN_CMD_TEST_CHILD";

/// Every variable the loader or the HTTP clients read from the process.
const MANAGED_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "GITHUB_TOKEN",
    "RSSHUB_BASE_URL",
    "RSSHUB_ACCESS_KEY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "DATABASE_URL",
];

/// Returns `true` in the child (run the test body there); in the parent it
/// runs `test` in a child whose environment holds exactly `vars` among
/// [`MANAGED_ENV`], with a working directory without `.env`, asserts the
/// child ran that one test successfully, and returns `false`.
fn run_in_isolated_child(test: &str, vars: &[(&str, &str)]) -> bool {
    if std::env::var_os(CHILD_MARKER).is_some() {
        return true;
    }
    let cwd = TempDir::new().expect("child cwd");
    let mut child = Process::new(std::env::current_exe().expect("test executable"));
    child
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, "1")
        .current_dir(cwd.path())
        .stdin(Stdio::null());
    for key in MANAGED_ENV {
        child.env_remove(key);
    }
    for (key, value) in vars {
        child.env(key, value);
    }
    let output = child.output().expect("spawn child test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child test {test} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    false
}

/// R3-5: a feed template that is valid, with `RSSHUB_BASE_URL` set but
/// invalid (no scheme), used to abort `run` with config error 78 before any
/// stage ran. It is an ingest precondition: ingest fails (78) and publish
/// still publishes the article already stored.
#[tokio::test]
async fn run_with_invalid_rsshub_base_url_still_publishes_stored_articles() {
    if !run_in_isolated_child(
        "run_with_invalid_rsshub_base_url_still_publishes_stored_articles",
        &[("RSSHUB_BASE_URL", "feeds.internal")],
    ) {
        return;
    }
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("rss.sqlite");
    write_config(temp.path(), &db_path, "{RSSHUB}/feed.xml");
    let pool = migrated_pool(&db_path).await;
    seed_persisted_article(&pool, "ai").await;
    pool.close().await;

    let cli = cli_for(temp.path());
    let summary = run::run(&cli, run_args(&cli))
        .await
        .expect("an ingest precondition is a stage failure, not a load error");

    assert!(summary.ingest.is_none(), "ingest must be skipped");
    assert_eq!(
        summary
            .stage_failures
            .iter()
            .map(|failure| (failure.stage, failure.exit_code_value))
            .collect::<Vec<_>>(),
        vec![("ingest", 78)],
        "{:?}",
        summary.stage_failures
    );
    assert!(
        summary.stage_failures[0]
            .message
            .contains("RSSHUB_BASE_URL"),
        "{:?}",
        summary.stage_failures
    );
    let publish = summary.publish.as_ref().expect("publish ran");
    let ai = publish
        .categories
        .iter()
        .find(|category| category.category == "ai")
        .expect("ai category published");
    assert_eq!(ai.items, 1, "the stored article is published: {publish:?}");
    assert_eq!(summary.exit_code(), ExitCode::ConfigError);
}

fn run_args(cli: &Cli) -> &RunArgs {
    match &cli.command {
        Command::Run(args) => args,
        _ => unreachable!("cli_for builds a run command"),
    }
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
        command: Command::Run(RunArgs {
            ingest_batch_size: None,
            ai_batch_size: None,
            publish_date: None,
            max_batches: Some(1),
        }),
    }
}

async fn migrated_pool(db_path: &Path) -> sqlx::SqlitePool {
    let pool = build_sqlite_pool(db_path, 1, 5_000).await.expect("pool");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrations");
    pool
}

async fn insert_rule(pool: &sqlx::SqlitePool, kind: &str, suffix: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) VALUES (?, ?, 'test', ?) RETURNING id",
    )
    .bind(kind)
    .bind(format!("{kind}-{suffix}"))
    .bind(format!("sha-{kind}-{suffix}"))
    .fetch_one(pool)
    .await
    .expect("rule")
}

/// One persisted article of `category`, published an hour ago.
async fn seed_persisted_article(pool: &sqlx::SqlitePool, category: &str) {
    let config = insert_rule(pool, "config", category).await;
    let extractor = insert_rule(pool, "extractor", category).await;
    let source_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, config_version) VALUES (?, 'seed', 'Seed', 'https://example.test/seed.xml', 'rss', ?) RETURNING id",
    )
    .bind(category)
    .bind(config)
    .fetch_one(pool)
    .await
    .expect("source");
    let published_at = OffsetDateTime::now_utc() - Duration::hours(1);
    let entry_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, title_raw, summary_raw, published_at, discovered_at, state, dedup_decision) VALUES (?, 'u', 'https://example.test/a', 'h', 'Title', 'Summary', ?, ?, 'persisted', 'fresh') RETURNING id",
    )
    .bind(source_id)
    .bind(published_at)
    .bind(published_at)
    .fetch_one(pool)
    .await
    .expect("entry");
    let article_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO articles (content_hash, canonical_link, title, body_text, extractor_strategy, extractor_version, content_quality, word_count, origin_feed_entry_id, state) VALUES ('c', 'https://example.test/a', 'Title', 'body', 'readability', ?, 'high', 1, ?, 'persisted') RETURNING id",
    )
    .bind(extractor)
    .bind(entry_id)
    .fetch_one(pool)
    .await
    .expect("article");
    sqlx::query("UPDATE feed_entries SET article_id = ? WHERE id = ?")
        .bind(article_id)
        .bind(entry_id)
        .execute(pool)
        .await
        .expect("link");
}

/// SQLite config, AI disabled, unscored articles published, one category
/// `ai` whose single source reads `feed_url`.
fn write_config(root: &Path, db_path: &Path, feed_url: &str) {
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
key = "src"
display_name = "Source"
feed_url = "{feed_url}"
feed_kind = "rss"
priority = 10
enabled = true
"#
        ),
    )
    .expect("write category");
}
