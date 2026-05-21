use std::{fs, path::Path};

use rss_ai_news_cli::{
    args::{Cli, Command, DoctorArgs, LogFormat, OutputFormat},
    commands::doctor,
    error::CliError,
    output::{CommandSummary, DoctorCommandSummary, OutputWriter},
};
use rss_ai_news_observability::health::{CheckOutcome, CheckReport};
use rss_ai_news_runtime::doctor::deep_scan::{
    DeepScanReport, InvariantId, InvariantResult, ViolationRow,
};
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use tempfile::TempDir;

#[tokio::test]
async fn doctor_cmd_shallow_non_failing_checks_return_success() {
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), temp.path().join("rss.sqlite").as_path());
    let cli = cli_for(temp.path(), false, OutputFormat::Pretty);
    let mut writer = OutputWriter::new(rss_ai_news_cli::output::OutputFormat::Pretty);

    doctor::run(&cli, doctor_args(&cli), &mut writer)
        .await
        .expect("doctor succeeds with warn/info only");
}

#[tokio::test]
async fn doctor_cmd_missing_github_token_is_not_failure() {
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), temp.path().join("rss.sqlite").as_path());
    let cli = cli_for(temp.path(), false, OutputFormat::Pretty);
    let mut writer = OutputWriter::new(rss_ai_news_cli::output::OutputFormat::Pretty);

    let result = doctor::run(&cli, doctor_args(&cli), &mut writer).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn doctor_cmd_uncreatable_database_path_returns_storage_error() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("missing-parent").join("rss.sqlite");
    write_config(temp.path(), &db_path);
    let cli = cli_for(temp.path(), false, OutputFormat::Pretty);
    let mut writer = OutputWriter::new(rss_ai_news_cli::output::OutputFormat::Pretty);

    let error = doctor::run(&cli, doctor_args(&cli), &mut writer)
        .await
        .expect_err("database path should fail");

    assert!(matches!(error, CliError::Storage(_)));
}

#[tokio::test]
async fn doctor_cmd_deep_happy_path_returns_success() {
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path(), temp.path().join("rss.sqlite").as_path());
    let cli = cli_for(temp.path(), true, OutputFormat::Pretty);
    let mut writer = OutputWriter::new(rss_ai_news_cli::output::OutputFormat::Pretty);

    doctor::run(&cli, doctor_args(&cli), &mut writer)
        .await
        .expect("deep doctor succeeds");
}

#[tokio::test]
async fn doctor_cmd_deep_i6_violation_returns_doctor_failed() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("rss.sqlite");
    write_config(temp.path(), &db_path);
    seed_i6_violation(&db_path).await;
    let cli = cli_for(temp.path(), true, OutputFormat::Pretty);
    let mut writer = OutputWriter::new(rss_ai_news_cli::output::OutputFormat::Pretty);

    let error = doctor::run(&cli, doctor_args(&cli), &mut writer)
        .await
        .expect_err("I6 should fail doctor");

    assert!(matches!(error, CliError::DoctorFailed));
}

#[test]
fn doctor_summary_pretty_snapshot_contains_status_lines() {
    let summary = DoctorCommandSummary::new(
        CheckReport {
            items: vec![
                (
                    "Configuration".to_string(),
                    CheckOutcome::Ok("valid".to_string()),
                ),
                (
                    "GitHub token".to_string(),
                    CheckOutcome::Warn("not configured".to_string()),
                ),
            ],
        },
        Some(deep_report(0)),
    );
    let mut out = Vec::new();
    summary.render_pretty(&mut out).expect("render");
    let text = String::from_utf8(out).expect("utf8");

    assert!(text.contains("[OK  ] Configuration valid"));
    assert!(text.contains("[WARN] GitHub token not configured"));
    assert!(text.contains("--- deep scan ---"));
    assert!(text.contains("[OK  ] I6 publish_records.published_* => articles.published"));
}

#[test]
fn doctor_summary_json_snapshot_has_command_status_and_checks() {
    let summary = DoctorCommandSummary::new(
        CheckReport {
            items: vec![(
                "Configuration".to_string(),
                CheckOutcome::Ok("valid".to_string()),
            )],
        },
        Some(deep_report(3)),
    );
    let envelope = serde_json::json!({
        "command": "doctor",
        "status": summary.status(),
        "summary": summary,
        "errors": [],
    });

    assert_eq!(envelope["command"], "doctor");
    assert_eq!(envelope["status"], "fail");
    assert_eq!(
        envelope["summary"]["shallow_checks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(envelope["summary"]["deep_scan"][0]["violations"], 3);
}

fn cli_for(config_dir: &Path, deep: bool, output_format: OutputFormat) -> Cli {
    Cli {
        config_dir: config_dir.to_path_buf(),
        db_path: None,
        log_level: "info".to_string(),
        log_format: LogFormat::Pretty,
        log_file: String::new(),
        metrics_bind: String::new(),
        output_format,
        dry_run: false,
        category: None,
        timezone: None,
        command: Command::Doctor(DoctorArgs { deep }),
    }
}

fn doctor_args(cli: &Cli) -> &DoctorArgs {
    match &cli.command {
        Command::Doctor(args) => args,
        _ => panic!("expected doctor command"),
    }
}

fn write_config(root: &Path, db_path: &Path) {
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

async fn seed_i6_violation(db_path: &Path) {
    let pool = build_sqlite_pool(db_path, 1, 5_000).await.expect("pool");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrations");
    let rule = insert_rule(&pool, "config").await;
    let extractor = insert_rule(&pool, "extractor").await;
    let prompt = insert_rule(&pool, "prompt").await;
    let schema = insert_rule(&pool, "ai_output_schema").await;
    let render = insert_rule(&pool, "render").await;
    let policy = insert_rule(&pool, "selection_policy").await;
    let source_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, config_version) VALUES ('ai', 's', 'S', 'https://example.test/feed.xml', 'rss', ?) RETURNING id",
    )
    .bind(rule)
    .fetch_one(&pool)
    .await
    .expect("source");
    let entry_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, title_raw, discovered_at, state, dedup_decision) VALUES (?, 'u', 'https://example.test/a', 'h', 't', datetime('now'), 'persisted', 'fresh') RETURNING id",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .expect("entry");
    let article_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO articles (content_hash, canonical_link, title, body_text, extractor_strategy, extractor_version, content_quality, word_count, origin_feed_entry_id, state) VALUES ('c', 'https://example.test/a', 't', 'body', 'readability', ?, 'high', 1, ?, 'ready_for_publish') RETURNING id",
    )
    .bind(extractor)
    .bind(entry_id)
    .fetch_one(&pool)
    .await
    .expect("article");
    sqlx::query("UPDATE feed_entries SET article_id = ? WHERE id = ?")
        .bind(article_id)
        .bind(entry_id)
        .execute(&pool)
        .await
        .expect("link");
    let ai_result_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO article_ai_results (article_id, prompt_version, output_schema_version, model_id, state, keep_decision) VALUES (?, ?, ?, 'm', 'succeeded', 1) RETURNING id",
    )
    .bind(article_id)
    .bind(prompt)
    .bind(schema)
    .fetch_one(&pool)
    .await
    .expect("ai");
    let publish_record_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO publish_records (idempotency_key, category_key, report_date, target_timezone, render_version, selection_policy_version, state) VALUES ('p', 'ai', '2026-04-30', 'Asia/Shanghai', ?, ?, 'published_remote') RETURNING id",
    )
    .bind(render)
    .bind(policy)
    .fetch_one(&pool)
    .await
    .expect("publish");
    sqlx::query(
        "INSERT INTO publish_items (publish_record_id, position, article_id, article_ai_result_id, frozen_title, frozen_summary, frozen_tags_json, frozen_score, frozen_canonical_link, frozen_source_display_name) VALUES (?, 1, ?, ?, 't', 's', '[]', 80, 'https://example.test/a', 'S')",
    )
    .bind(publish_record_id)
    .bind(article_id)
    .bind(ai_result_id)
    .execute(&pool)
    .await
    .expect("item");
}

async fn insert_rule(pool: &sqlx::SqlitePool, kind: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) VALUES (?, ?, 'test', ?) RETURNING id",
    )
    .bind(kind)
    .bind(format!("{kind}-{}", uuidish()))
    .bind(format!("sha-{}", uuidish()))
    .fetch_one(pool)
    .await
    .expect("rule")
}

fn deep_report(violations: u64) -> DeepScanReport {
    DeepScanReport {
        results: vec![InvariantResult {
            id: InvariantId::I6,
            total_count: violations,
            violations: if violations == 0 {
                Vec::new()
            } else {
                vec![ViolationRow {
                    primary_id: 42,
                    message: "publish_record_id=42 article_id=701 article.state=ready_for_publish"
                        .to_string(),
                }]
            },
        }],
    }
}

fn uuidish() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
        .to_string()
}
