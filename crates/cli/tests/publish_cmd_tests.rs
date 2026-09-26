//! Command-level counterexamples for multi-category commands (`publish`,
//! `publish-all`, `ai-run`): record isolation across categories and failure
//! propagation into the summary / exit code.

use std::{fs, path::Path};

use rss_ai_news_cli::{
    args::{AiRunArgs, Cli, Command, LogFormat, OutputFormat, PublishArgs},
    commands::{
        ai_run,
        publish::{self, StageVerdict},
        publish_all,
    },
    exit_code::ExitCode,
    output::CommandSummary,
};
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};

const REPORT_DATE: &str = "2026-05-18";

#[tokio::test]
async fn publish_category_never_claims_another_categorys_pending_record() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("rss.sqlite");
    write_config(temp.path(), &db_path, &temp.path().join("output"));
    let pool = migrated_pool(&db_path).await;
    let foreign_id = insert_pending_record(&pool, "math").await;
    pool.close().await;

    let cli = cli_for(temp.path(), "ai");
    let summary = publish::run(&cli, publish_args(&cli))
        .await
        .expect("publish runs");

    assert_eq!(summary.category, "ai");
    assert_ne!(summary.publish_record_id, foreign_id);

    let pool = build_sqlite_pool(&db_path, 1, 5_000).await.expect("pool");
    let (category, state, attempts, lease_owner) = sqlx::query_as::<
        _,
        (String, String, i64, Option<String>),
    >(
        "SELECT category_key, state, attempt_count, lease_owner FROM publish_records WHERE id = ?",
    )
    .bind(foreign_id)
    .fetch_one(&pool)
    .await
    .expect("foreign record");
    assert_eq!(category, "math");
    assert_eq!(state, "pending", "foreign record must stay untouched");
    assert_eq!(attempts, 0, "foreign record must not be claimed");
    assert_eq!(lease_owner, None);
}

#[tokio::test]
async fn publish_store_local_failure_is_reported_as_failure() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("rss.sqlite");
    // A regular file where the output directory's parent should be: every
    // write below it fails with ENOTDIR, independent of uid/permissions.
    let blocker = temp.path().join("blocker");
    fs::write(&blocker, b"not a directory").expect("blocker file");
    write_config(temp.path(), &db_path, &blocker.join("out"));
    let pool = migrated_pool(&db_path).await;
    seed_persisted_article(&pool, "ai").await;
    pool.close().await;

    let mut cli = cli_for(temp.path(), "ai");
    cli.command = Command::Publish(PublishArgs {
        date: None,
        local_only: true,
        force: false,
    });
    let summary = publish::run(&cli, publish_args(&cli))
        .await
        .expect("stage failures are reported through the summary");

    let store = summary
        .stages
        .iter()
        .find(|stage| stage.stage == "store_local")
        .expect("store_local stage ran");
    assert_eq!(store.verdict, StageVerdict::Failed, "{store:?}");
    assert_eq!(summary.exit_code(), ExitCode::RuntimeError);
    assert_eq!(summary.status(), "fail");
    assert_eq!(summary.errors()[0].kind, "publish_store_local");
}

#[tokio::test]
async fn publish_all_records_a_category_conflict_and_keeps_other_categories() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("rss.sqlite");
    write_config(temp.path(), &db_path, &temp.path().join("output"));
    let pool = migrated_pool(&db_path).await;
    seed_persisted_article(&pool, "ai").await;
    pool.close().await;

    let mut cli = cli_for(temp.path(), "ai");
    cli.category = None;
    cli.command = Command::PublishAll(PublishArgs {
        date: None,
        local_only: true,
        force: false,
    });
    let args = match &cli.command {
        Command::PublishAll(args) => args,
        _ => unreachable!(),
    };

    // First run: ai publishes; math has no candidates and stays pending.
    let first = publish_all::run(&cli, args).await.expect("first run");
    assert_eq!(first.exit_code(), ExitCode::Success, "{first:?}");

    // Make math's record for today terminal (e.g. retry budget exhausted).
    let pool = build_sqlite_pool(&db_path, 1, 5_000).await.expect("pool");
    sqlx::query("UPDATE publish_records SET state = 'failed' WHERE category_key = 'math'")
        .execute(&pool)
        .await
        .expect("mark math failed");
    pool.close().await;

    // Second run: math's terminal record is a conflict. It must be reported
    // for math only, not abort the whole command.
    let second = publish_all::run(&cli, args)
        .await
        .expect("category conflicts are reported through the summary");
    assert_eq!(second.categories.len(), 2, "{second:?}");
    let math = second
        .categories
        .iter()
        .find(|category| category.category == "math")
        .expect("math summary");
    assert!(
        math.stages
            .iter()
            .any(|stage| stage.stage == "conflict" && stage.verdict == StageVerdict::Failed)
    );
    assert!(
        second
            .categories
            .iter()
            .any(|category| category.category == "ai")
    );
    assert_eq!(second.exit_code(), ExitCode::RuntimeError);
}

#[tokio::test]
async fn ai_run_without_category_runs_every_category_and_reports_each_failure() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("rss.sqlite");
    write_config_with(temp.path(), &db_path, &temp.path().join("output"), true);

    let mut cli = cli_for(temp.path(), "ai");
    cli.category = None;
    cli.command = Command::AiRun(AiRunArgs::default());
    let args = match &cli.command {
        Command::AiRun(args) => args,
        _ => unreachable!(),
    };
    // Previously: Err("category is required when multiple ... categories"),
    // so `run` never processed AI with more than one category.
    let summary = ai_run::run(&cli, args)
        .await
        .expect("multi-category ai-run reports per-category failures");

    let failed = summary
        .category_failures
        .iter()
        .map(|failure| failure.category.as_str())
        .collect::<Vec<_>>();
    assert_eq!(failed, ["ai", "math"], "{summary:?}");
    // Missing credentials are a config error for each category.
    assert_eq!(summary.exit_code(), ExitCode::ConfigError);
    assert_eq!(summary.errors().len(), 2);
}

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

async fn migrated_pool(db_path: &Path) -> sqlx::SqlitePool {
    let pool = build_sqlite_pool(db_path, 1, 5_000).await.expect("pool");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrations");
    pool
}

async fn insert_pending_record(pool: &sqlx::SqlitePool, category: &str) -> i64 {
    let render = insert_rule(pool, "render", category).await;
    let selection = insert_rule(pool, "selection_policy", category).await;
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO publish_records (idempotency_key, category_key, report_date, target_timezone, render_version, selection_policy_version, state) VALUES (?, ?, ?, 'Asia/Shanghai', ?, ?, 'pending') RETURNING id",
    )
    .bind(format!("foreign-{category}"))
    .bind(category)
    .bind(REPORT_DATE)
    .bind(render)
    .bind(selection)
    .fetch_one(pool)
    .await
    .expect("insert publish record")
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

fn cli_for(config_dir: &Path, category: &str) -> Cli {
    Cli {
        config_dir: config_dir.to_path_buf(),
        db_path: None,
        log_level: "info".to_string(),
        log_format: LogFormat::Pretty,
        log_file: String::new(),
        metrics_bind: String::new(),
        output_format: OutputFormat::Json,
        dry_run: false,
        category: Some(category.to_string()),
        timezone: None,
        command: Command::Publish(PublishArgs {
            date: Some(REPORT_DATE.to_string()),
            local_only: true,
            force: false,
        }),
    }
}

fn publish_args(cli: &Cli) -> &PublishArgs {
    match &cli.command {
        Command::Publish(args) => args,
        _ => panic!("expected publish command"),
    }
}

fn write_config(root: &Path, db_path: &Path, output_dir: &Path) {
    write_config_with(root, db_path, output_dir, false);
}

/// `ai_enabled = true` also points every category at an API-key env var that
/// is never set, so credential resolution fails per category without network.
fn write_config_with(root: &Path, db_path: &Path, output_dir: &Path, ai_enabled: bool) {
    fs::create_dir_all(root.join("categories")).expect("create categories");
    let db_path = db_path.to_string_lossy().replace('\\', "/");
    let output_dir = output_dir.to_string_lossy().replace('\\', "/");
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
enabled = {ai_enabled}
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
frontmatter_template = "---\ntitle: {{date}}\ndate: {{date}}\nexcerpt: {{excerpt_yaml}}\n---\n"
report_template = "{{frontmatter}}\n# {{title_md}}\n{{excerpt_block}}\n{{items}}"
item_template = '''
## {{item_title_md}}{{score_badge}}

- **Source:** `{{source_code}}` | [link]({{url_md}})

{{summary_blockquote}}

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

    let ai_override = if ai_enabled {
        "\n[category.ai_override]\napi_key_env = \"RSS_AI_NEWS_TEST_UNSET_KEY\"\n"
    } else {
        ""
    };
    for (key, name, priority) in [("ai", "AI", 10), ("math", "Math", 20)] {
        fs::write(
            root.join("categories").join(format!("{key}.toml")),
            format!(
                r#"
schema_version = "1"

[category]
key = "{key}"
display_name = "{name}"
priority = {priority}
{ai_override}
[[sources]]
key = "{key}-mock"
display_name = "Mock {name}"
feed_url = "https://example.test/{key}.xml"
feed_kind = "rss"
priority = 10
enabled = true
"#
            ),
        )
        .expect("write category");
    }
}
