//! `decision-run` (ADR 0011 §11): configuration checks exit 78 before any
//! storage is opened; an enabled run against a mock laya-server records the
//! answer.

mod support;

use std::fs;
use std::path::Path;

use clap::Parser;
use rss_ai_news_cli::{
    args::{Cli, Command, DecisionRunArgs},
    commands::{check_invocation, decision_run},
    error::CliError,
    exit_code::ExitCode,
    output::CommandSummary,
};
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use support::{cli_for, write_config};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const QUESTIONS: &str = r#"
[[category.decision.questions]]
id = "q_marketing"
type = "noul"
instructions = "这篇是否主要是营销稿？"
"#;

fn append(path: &Path, text: &str) {
    let mut content = fs::read_to_string(path).unwrap();
    content.push_str(text);
    fs::write(path, content).unwrap();
}

/// `api_key_env = "PATH"`: always set, so no test touches the process
/// environment; the value only ever goes to the local mock.
fn decision_section(base_url: &str, extra: &str) -> String {
    format!(
        r#"
[decision]
enabled = true
provider = "laya"
base_url = "{base_url}"
api_key_env = "PATH"
model = "multilingual"
{extra}
"#
    )
}

fn config(app_section: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    write_config(temp.path(), true, false);
    append(&temp.path().join("categories").join("ai.toml"), QUESTIONS);
    append(&temp.path().join("app.toml"), app_section);
    temp
}

fn decision_cli(config_dir: &Path, category: Option<&str>) -> (Cli, DecisionRunArgs) {
    let args = DecisionRunArgs::default();
    let mut cli = cli_for(config_dir);
    cli.command = Command::DecisionRun(args.clone());
    cli.category = category.map(str::to_string);
    (cli, args)
}

async fn expect_config_error(app_section: &str, category: Option<&str>) -> String {
    let temp = config(app_section);
    let (cli, args) = decision_cli(temp.path(), category);
    let error = decision_run::run(&cli, &args)
        .await
        .expect_err("configuration error");
    assert_eq!(error.exit_code(), ExitCode::ConfigError, "{error}");
    assert!(
        !temp.path().join("rss.sqlite").exists(),
        "storage must not be opened"
    );
    error.to_string()
}

#[tokio::test]
async fn a_missing_or_disabled_section_exits_78_without_storage() {
    expect_config_error("", None).await;
    expect_config_error("\n[decision]\nenabled = false\nprovider = \"laya\"\n", None).await;
}

#[tokio::test]
async fn a_missing_key_exits_78_without_naming_its_value() {
    let section = decision_section("http://localhost:9", "")
        .replace("\"PATH\"", "\"RSS_AI_NEWS_TEST_UNSET_DECISION_KEY\"");
    let message = expect_config_error(&section, None).await;
    assert!(
        message.contains("RSS_AI_NEWS_TEST_UNSET_DECISION_KEY"),
        "{message}"
    );
}

#[tokio::test]
async fn a_budget_below_the_first_round_exits_78_unless_one_category_is_selected() {
    let section = decision_section("http://localhost:9", "max_requests_per_run = 1");
    let message = expect_config_error(&section, None).await;
    assert!(message.contains("--category"), "{message}");
    // One selected category needs no first-round reservation.
    let temp = config(&section);
    let (cli, args) = decision_cli(temp.path(), Some("ai"));
    let summary = decision_run::run(&cli, &args).await.unwrap();
    assert_eq!((summary.calls, summary.exit_code()), (0, ExitCode::Success));
}

#[tokio::test]
async fn dry_run_is_rejected() {
    let cli = Cli::try_parse_from(["rss-ai-news", "--dry-run", "decision-run"]).unwrap();
    assert!(matches!(
        check_invocation(&cli),
        Err(CliError::DryRunUnsupported {
            command: "decision-run"
        })
    ));
    assert!(Cli::try_parse_from(["rss-ai-news", "decision-run", "--max-requests", "0"]).is_err());
}

async fn seed_article(db_path: &Path) {
    let pool = build_sqlite_pool(db_path, 1, 5000).await.unwrap();
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .unwrap();
    let rule: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', 'ext', 'e', 'sha-ext', 'superseded') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let config: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('config', 'cfg', 'c', 'sha-cfg', 'superseded') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let source: i64 = sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, config_version) \
         VALUES ('ai', 'mock', 'Mock', 'https://example.test/feed.xml', 'rss', ?) RETURNING id",
    )
    .bind(config)
    .fetch_one(&pool)
    .await
    .unwrap();
    let entry: i64 = sqlx::query_scalar(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, title_raw, \
         discovered_at, state, dedup_decision) \
         VALUES (?, 'uid', 'https://example.test/a', 'hash', 'title', ?, 'pending_fetch', 'fresh') RETURNING id",
    )
    .bind(source)
    .bind(time::OffsetDateTime::now_utc())
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO articles (content_hash, canonical_link, title, body_text, extractor_strategy, \
         extractor_version, content_quality, origin_feed_entry_id) \
         VALUES ('c', 'https://example.test/a', '标题', '正文', 'readability', ?, 'high', ?)",
    )
    .bind(rule)
    .bind(entry)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn an_enabled_run_records_the_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "laya-rl-agent",
            "answers": {
                "q_marketing": {"type": "noul", "noul": 0.1, "confidence": 0.9, "answer_confidence": 0.9}
            },
            "usage": {"input_tokens": 50, "state_tokens_dropped": 0, "truncated": false},
            "routing": {"model": "multilingual"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    // `localhost` matches a developer NO_PROXY as a domain; CI has no proxy.
    let base = server.uri().replace("127.0.0.1", "localhost");
    let temp = config(&decision_section(&base, ""));
    seed_article(&temp.path().join("rss.sqlite")).await;
    let (cli, args) = decision_cli(temp.path(), None);
    let summary = decision_run::run(&cli, &args).await.unwrap();
    assert_eq!(summary.exit_code(), ExitCode::Success);
    assert_eq!((summary.calls, summary.claimed), (1, 1));
    assert_eq!(summary.execution_key, "laya:multilingual:2048");
    let value = serde_json::to_value(&summary).unwrap();
    assert_eq!(value["categories"][0]["succeeded"], 1);
    let pool = build_sqlite_pool(&temp.path().join("rss.sqlite"), 1, 5000)
        .await
        .unwrap();
    let (state, answers): (String, String) =
        sqlx::query_as("SELECT state, answers_json FROM decision_results")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "succeeded");
    assert!(answers.contains("\"p_yes\":0.1"), "{answers}");
}
