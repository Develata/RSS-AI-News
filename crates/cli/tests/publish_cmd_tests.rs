//! Command-level counterexamples for `publish`: record isolation across
//! categories and failure propagation into the summary / exit code.

use std::{fs, path::Path};

use rss_ai_news_cli::{
    args::{Cli, Command, LogFormat, OutputFormat, PublishArgs},
    commands::publish,
};
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use tempfile::TempDir;

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
