//! Storage composition-root regression tests: config rotation stays explicit,
//! and opening write storage does not require AI clients or credentials.
//! LoadedConfig is constructed directly to avoid process-environment pollution.

use rss_ai_news_cli::context_factory::open_write_storage;
use rss_ai_news_config::{
    AppConfig, CategoryConfig, CliOverrides, EnvConfig, LoadedConfig, SourceSecrets,
};
use rss_ai_news_domain::SecretString;
use rss_ai_news_storage::{
    RuleVersionRepo, RuleVersionRepository, StoragePool, build_sqlite_pool, run_migrations,
};
use tempfile::TempDir;

#[tokio::test]
async fn opening_write_storage_does_not_require_complete_ai_credentials() {
    let temp = TempDir::new().expect("temp dir");
    let loaded = loaded_config(&temp);
    assert!(loaded.env.openai_api_key.is_some());
    assert!(loaded.env.openai_base_url.is_none());

    open_write_storage(&loaded)
        .await
        .expect("storage startup must be independent of incomplete AI credentials");
}

/// W16 P2（docs/plan/16-config-versioning.md §5/§7）：启动期 seed 的 sha-keyed
/// 轮换接线——config 漂移后 active 行跟随最近一次启动的 sha。
#[tokio::test]
async fn startup_seed_rotates_active_config_to_current_sha() {
    let temp = TempDir::new().expect("temp dir");

    let mut loaded = loaded_config(&temp);
    loaded.config_sha256 = "a".repeat(64);
    open_write_storage(&loaded)
        .await
        .expect("first build seeds sha A");

    let mut loaded = loaded_config(&temp);
    loaded.config_sha256 = "b".repeat(64);
    open_write_storage(&loaded)
        .await
        .expect("second build rotates to sha B");

    let pool = verify_pool(&temp).await;
    let active = RuleVersionRepo::new(pool.clone())
        .active_rule("config")
        .await
        .expect("active_rule query")
        .expect("active config row should exist");
    assert_eq!(
        active.payload_sha256,
        "b".repeat(64),
        "active config 必须跟随最近一次启动的 sha"
    );
    let superseded: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM rule_versions
         WHERE kind='config' AND status='superseded' AND payload_sha256=?",
    )
    .bind("a".repeat(64))
    .fetch_one(&pool)
    .await
    .expect("superseded count");
    assert_eq!(superseded, 1, "旧 sha 行应被 demote 为 superseded");
}

/// W16 P2：D1 存量库自愈——bootstrap placeholder 占着 active 时，CLI 启动
/// 必须把它收编为 superseded 并让真实 sha 接管。
#[tokio::test]
async fn startup_seed_supersedes_bootstrap_placeholder() {
    let temp = TempDir::new().expect("temp dir");

    // 预置 D1 场景：placeholder 是首个 active config 行。
    let pool = verify_pool(&temp).await;
    RuleVersionRepo::new(pool.clone())
        .active_rule_or_register(
            "config",
            "ingest-bootstrap",
            "auto-registered by ingest when no active config rule existed",
            "ingest-bootstrap",
        )
        .await
        .expect("placeholder seed");

    let mut loaded = loaded_config(&temp);
    loaded.config_sha256 = "c".repeat(64);
    open_write_storage(&loaded)
        .await
        .expect("build on placeholder db");

    let active = RuleVersionRepo::new(pool.clone())
        .active_rule("config")
        .await
        .expect("active_rule query")
        .expect("active config row should exist");
    assert_eq!(
        active.payload_sha256,
        "c".repeat(64),
        "placeholder 滞留库启动后真实 sha 必须接管 active"
    );
}

/// 打开与 open_write_storage 同一 sqlite 文件的校验连接（顺带跑 migrations，
/// 供"先预置数据再启动"的场景使用；migrations 幂等）。
async fn verify_pool(temp: &TempDir) -> sqlx::SqlitePool {
    let db_path = temp.path().join("rss.sqlite");
    let pool = build_sqlite_pool(&db_path, 1, 5_000)
        .await
        .expect("verify pool");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrations apply");
    pool
}

/// 密闭构造：所有路径落在 temp 目录，EnvConfig 逐字段赋值（不读进程 env、
/// 不读 .env 文件），database_url 留 None 让 resolve_storage_url 走
/// `sqlite://<sqlite_path>` fallback。
fn loaded_config(temp: &TempDir) -> LoadedConfig {
    let db_path = temp
        .path()
        .join("rss.sqlite")
        .to_string_lossy()
        .replace('\\', "/");
    let output_dir = temp
        .path()
        .join("output")
        .to_string_lossy()
        .replace('\\', "/");
    let artifact_dir = temp
        .path()
        .join("artifacts")
        .to_string_lossy()
        .replace('\\', "/");

    let app: AppConfig = toml::from_str(&format!(
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
    ))
    .expect("app fixture parses");

    let category: CategoryConfig = toml::from_str(
        r#"
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[category.ai_override]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
"#,
    )
    .expect("category fixture parses");

    let mut env = EnvConfig::default();
    env.openai_api_key = Some(SecretString::new("sk-legacy-global"));
    // openai_base_url / database_url 保持 None：前者是本回归的核心前提，
    // 后者让 storage 走 sqlite path fallback。

    LoadedConfig {
        env,
        app,
        categories: vec![category],
        source_secrets: SourceSecrets::default(),
        config_sha256: String::new(),
        cli_overrides: CliOverrides::default(),
    }
}
