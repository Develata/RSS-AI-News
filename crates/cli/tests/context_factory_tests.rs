//! W14-B codex P2 回归：composition root 全局 AI client 守卫。
//!
//! 放宽全局凭证 gate 后，"全部板块自带凭证 + 遗留全局 OPENAI_API_KEY +
//! 全局 OPENAI_BASE_URL 缺省"是合法配置；build_run_context 的全局分支
//! 必须同时校验 key + base，否则会拿空串 api_base 构造 OpenAiCompatClient
//! → InvalidConfig，让 ingest/publish 等不调 AI 的命令死在 ctx 构造期。

use std::{fs, path::Path};

use rss_ai_news_cli::context_factory::build_run_context;
use rss_ai_news_config::{self as config, CliOverrides};
use tempfile::TempDir;

#[tokio::test]
async fn global_branch_without_base_url_falls_back_to_null_client() {
    let temp = TempDir::new().expect("temp dir");
    write_config(temp.path());
    let env_file = temp.path().join("test.env");
    // 遗留全局 key 存在、全局 base 缺省；板块 key 由 DEEPSEEK_API_KEY 提供。
    fs::write(
        &env_file,
        "OPENAI_API_KEY=sk-legacy-global\nDEEPSEEK_API_KEY=sk-deepseek\n",
    )
    .expect("write env file");

    let loaded = config::load(temp.path(), Some(&env_file), CliOverrides::default())
        .expect("self-credentialed categories pass the relaxed global gate");

    if loaded.env.openai_base_url.is_some() {
        // 进程环境里有 OPENAI_BASE_URL 时（开发机），空 base 路径不可达，
        // 本回归断言失去意义——跳过而非误报。
        eprintln!("skip: OPENAI_BASE_URL present in process env");
        return;
    }

    // 修复前：全局分支只查 key → 空串 api_base → InvalidConfig 直接失败。
    build_run_context("test-global", &loaded, None)
        .await
        .expect("incomplete global credentials must fall back to NullAiClient, not fail");

    // Some(板块凭证) 路径照常装配。
    let credentials = loaded
        .ai_credentials_for_category("ai")
        .expect("category credentials resolve");
    build_run_context("test-category", &loaded, Some(credentials))
        .await
        .expect("category credentials build the client");
}

fn write_config(root: &Path) {
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
enabled = true
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

[category.ai_override]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"

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
