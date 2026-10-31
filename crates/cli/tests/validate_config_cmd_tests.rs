mod support;

use std::fs;
use std::path::Path;

use rss_ai_news_cli::{commands::validate_config, error::CliError, exit_code::ExitCode};
use support::{cli_for, write_config};
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

#[tokio::test]
async fn validate_config_warns_when_category_prompt_omits_article_placeholders() {
    let temp = TempDir::new().unwrap();
    write_config(temp.path(), true, false);
    let app_path = temp.path().join("app.toml");
    let app = fs::read_to_string(&app_path)
        .unwrap()
        .replace("enabled = false", "enabled = true");
    fs::write(app_path, app).unwrap();
    // Category-owned credentials keep the run independent of the process env;
    // `{{body}}` mirrors the double-brace syntax once shown in docs/plan/03-ai.md.
    let category_path = temp.path().join("categories").join("ai.toml");
    let category = fs::read_to_string(&category_path).unwrap().replace(
        "priority = 10\n\n[[sources]]",
        "priority = 10\n\n[category.ai_override]\n\
         prompt_template = \"secret-ish prompt: {{body}}\"\n\
         api_key_env = \"PATH\"\n\
         base_url = \"https://ai.example.test/v1\"\n\n[[sources]]",
    );
    fs::write(&category_path, category).unwrap();

    let summary = validate_config::run(&cli_for(temp.path())).await.unwrap();
    let mut pretty = Vec::new();
    rss_ai_news_cli::output::CommandSummary::render_pretty(&summary, &mut pretty).unwrap();
    let pretty = String::from_utf8(pretty).unwrap();
    assert!(
        pretty.contains(
            "Warning [prompt_template] category.ai_override.prompt_template (category ai): \
             Missing {body_text}"
        ),
        "{pretty}"
    );
    let value = serde_json::to_value(&summary).unwrap();
    let prompt_warnings: Vec<_> = value["warnings"]
        .as_array()
        .expect("structured warnings")
        .iter()
        .filter(|warning| warning["code"] == "prompt_template")
        .collect();
    assert_eq!(prompt_warnings.len(), 3, "{prompt_warnings:?}");
    assert!(
        prompt_warnings
            .iter()
            .all(|warning| warning["category"] == "ai")
    );
    assert!(!value.to_string().contains("secret-ish prompt"));
}

const DECISION_QUESTIONS: &str = r#"
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

fn decision_config(app_section: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    write_config(temp.path(), true, false);
    append(
        &temp.path().join("categories").join("ai.toml"),
        DECISION_QUESTIONS,
    );
    append(&temp.path().join("app.toml"), app_section);
    temp
}

#[tokio::test]
async fn validate_config_warns_for_decision_questions_without_an_enabled_section() {
    let temp = decision_config("");
    let summary = validate_config::run(&cli_for(temp.path())).await.unwrap();
    let value = serde_json::to_value(&summary).unwrap();
    let inert: Vec<_> = value["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|warning| warning["code"] == "decision_inert")
        .collect();
    assert_eq!(inert.len(), 1, "{value}");
    assert_eq!(inert[0]["category"], "ai");
}

#[tokio::test]
async fn validate_config_checks_the_decision_key_only_when_enabled() {
    let enabled = r#"
[decision]
enabled = true
provider = "laya"
base_url = "http://laya.internal:8080"
api_key_env = "RSS_AI_NEWS_TEST_UNSET_DECISION_KEY"
model = "multilingual"
"#;
    let error = validate_config::run(&cli_for(decision_config(enabled).path()))
        .await
        .expect_err("missing decision key");
    assert_eq!(error.exit_code(), ExitCode::ConfigError);
    assert!(
        error
            .to_string()
            .contains("RSS_AI_NEWS_TEST_UNSET_DECISION_KEY")
    );
    // PATH is always set; no decision warning remains once enabled.
    let present = enabled.replace("RSS_AI_NEWS_TEST_UNSET_DECISION_KEY", "PATH");
    let summary = validate_config::run(&cli_for(decision_config(&present).path()))
        .await
        .unwrap();
    let value = serde_json::to_value(&summary).unwrap();
    assert!(!value.to_string().contains("decision_"), "{value}");
    // Disabled: the unset variable is never looked up.
    let disabled = enabled.replace("enabled = true", "enabled = false");
    validate_config::run(&cli_for(decision_config(&disabled).path()))
        .await
        .unwrap();
}

#[tokio::test]
async fn validate_config_rejects_laya_auto_routing() {
    let auto = "\n[decision]\nprovider = \"laya\"\nmodel = \"auto\"\n";
    let error = validate_config::run(&cli_for(decision_config(auto).path()))
        .await
        .expect_err("auto is per-input routing");
    assert_eq!(error.exit_code(), ExitCode::ConfigError);
    assert!(error.to_string().contains("decision.model"));
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
