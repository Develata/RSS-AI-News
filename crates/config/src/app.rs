use std::{num::NonZeroU32, path::PathBuf};

use rss_ai_news_domain::Score0To100;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    pub schema_version: String,
    pub database: DatabaseConfig,
    pub http: HttpConfig,
    pub ai: AiConfig,
    pub publish: PublishConfig,
    pub dedup: DedupConfig,
    pub extractor: ExtractorConfig,
    pub lease: LeaseConfig,
    pub retry: RetryConfig,
    /// 单次 run 工作量边界。`#[serde(default)]` 使旧 `app.toml`
    /// （无 `[runtime]` 段）仍可解析；缺省值见 [config-schema §4.4](docs/design/config-schema.md#44-runtime-字段语义)。
    #[serde(default)]
    pub runtime: RuntimeConfig,
    pub artifact: ArtifactConfig,
    pub observability: ObservabilityConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseConfig {
    pub driver: DatabaseDriver,
    pub sqlite_path: PathBuf,
    pub max_connections: u32,
    pub busy_timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseDriver {
    Sqlite,
    Postgres,
}

#[derive(Clone, Debug, Deserialize)]
pub struct HttpConfig {
    pub user_agent: String,
    pub timeout_seconds: u64,
    pub max_retries: u32,
    pub retry_backoff_base_ms: u64,
    pub concurrent_feeds: u32,
    pub concurrent_fetches: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AiConfig {
    pub enabled: bool,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub request_timeout_seconds: u64,
    pub max_input_chars: u32,
    pub rate_limit: AiRateLimitConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AiRateLimitConfig {
    pub requests_per_minute: u32,
    pub tokens_per_minute: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PublishConfig {
    pub target_timezone: String,
    pub github_owner: String,
    pub github_repo: String,
    pub github_branch: String,
    pub github_path_prefix: String,
    pub local_output_dir: PathBuf,
    pub include_unscored: bool,
    /// Global default for `[publish] max_items_per_report`. Categories may
    /// override per-field via `[category.publish_override]`. NonZeroU32 makes
    /// `0` (which would silently mean SQL `LIMIT 0`) a toml deserialization
    /// error. See `docs/design/config-schema.md` §4.5 / §234.
    pub max_items_per_report: NonZeroU32,
    /// Global default for `[publish] min_importance_score`. AI path: articles
    /// with score < this are filtered before report selection. AI-off direct
    /// path: not applied (see §4.5). Score0To100 enforces the 0-100 invariant
    /// at toml deserialization. See `docs/design/config-schema.md` §357-358.
    pub min_importance_score: Score0To100,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DedupConfig {
    pub enable_link_dedup: bool,
    pub enable_content_dedup: bool,
    pub link_normalizer_version: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExtractorConfig {
    pub strategy_order: Vec<String>,
    pub max_body_bytes: u64,
    pub min_body_chars: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LeaseConfig {
    pub fetch_duration_seconds: u64,
    pub ai_duration_seconds: u64,
    pub publish_duration_seconds: u64,
    pub reclaim_interval_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RetryConfig {
    pub feed_entry_max_attempts: u32,
    pub ai_max_attempts: u32,
    pub publish_max_attempts: u32,
}

/// `[runtime]` 段：单次 run 内部批次循环上限。
///
/// `max_batches_per_run` 在 ingest / ai-run 内部生效（每批 `--batch-size`
/// 行 × 上限批次 = 单次 run 处理上限）；`0` 表示不限，由 lease + 宿主超时兜底。
/// CLI `--max-batches` 覆盖此值。详见
/// [config-schema §4.4](docs/design/config-schema.md#44-runtime-字段语义)。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// 单次 run 内部批次循环上限。默认 10；`0` = 不限。
    #[serde(default = "RuntimeConfig::default_max_batches_per_run")]
    pub max_batches_per_run: u32,
}

impl RuntimeConfig {
    const fn default_max_batches_per_run() -> u32 {
        10
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_batches_per_run: Self::default_max_batches_per_run(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ArtifactConfig {
    pub retention_policy: RetentionPolicy,
    pub sample_rate: f32,
    pub inline_threshold_bytes: u64,
    pub file_storage_dir: PathBuf,
    pub ttl_days: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPolicy {
    Always,
    OnFailure,
    Sampled,
    DebugOnly,
    Off,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ObservabilityConfig {
    pub log_level: String,
    pub log_format: String,
    pub log_file: String,
    pub enable_metrics: bool,
    pub metrics_bind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_app_config() {
        let content = include_str!("../../../configs/app.toml.example");
        let config: AppConfig = toml::from_str(content).expect("example app config parses");

        assert_eq!(config.schema_version, "1");
        assert_eq!(config.database.driver, DatabaseDriver::Sqlite);
        assert_eq!(config.ai.model, "gpt-4o-mini");
        assert_eq!(config.ai.rate_limit.tokens_per_minute, 0);
        assert!(!config.publish.include_unscored);
        assert_eq!(config.runtime.max_batches_per_run, 10);
    }

    #[test]
    fn missing_runtime_block_falls_back_to_default() {
        // §4.4 line 196: 默认 10。`[runtime]` 段缺失时 serde default 必须
        // 还原默认值，不应反序列化失败 —— 旧 toml 应平滑兼容。
        let content = include_str!("../../../configs/app.toml.example");
        // 删掉 [runtime] 段，模拟旧配置
        let stripped: String = content
            .lines()
            .filter(|line| !line.starts_with("[runtime]") && !line.starts_with("max_batches_per_run"))
            .collect::<Vec<_>>()
            .join("\n");
        let config: AppConfig =
            toml::from_str(&stripped).expect("legacy app config without [runtime] parses");
        assert_eq!(config.runtime.max_batches_per_run, 10);
    }

    #[test]
    fn runtime_max_batches_zero_round_trips_as_unlimited() {
        // §4.4 line 196: `0` 表示不限。反序列化必须保留 0，不可被 default 顶替。
        let toml = r#"
schema_version = "1"
[database]
driver = "sqlite"
sqlite_path = "data.db"
max_connections = 5
busy_timeout_ms = 5000
[http]
user_agent = "x"
timeout_seconds = 30
max_retries = 3
retry_backoff_base_ms = 1000
concurrent_feeds = 10
concurrent_fetches = 5
[ai]
enabled = true
model = "m"
max_tokens = 100
temperature = 0.1
request_timeout_seconds = 60
max_input_chars = 1000
[ai.rate_limit]
requests_per_minute = 60
tokens_per_minute = 0
[publish]
target_timezone = "UTC"
github_owner = ""
github_repo = ""
github_branch = "main"
github_path_prefix = "x"
local_output_dir = "out"
include_unscored = false
max_items_per_report = 1
min_importance_score = 30
[dedup]
enable_link_dedup = true
enable_content_dedup = true
link_normalizer_version = "1"
[extractor]
strategy_order = ["readability"]
max_body_bytes = 1024
min_body_chars = 1
[lease]
fetch_duration_seconds = 1
ai_duration_seconds = 1
publish_duration_seconds = 1
reclaim_interval_seconds = 1
[retry]
feed_entry_max_attempts = 1
ai_max_attempts = 1
publish_max_attempts = 1
[runtime]
max_batches_per_run = 0
[artifact]
retention_policy = "off"
sample_rate = 0.1
inline_threshold_bytes = 1024
file_storage_dir = "x"
ttl_days = 30
[observability]
log_level = "info"
log_format = "pretty"
log_file = ""
enable_metrics = false
metrics_bind = "127.0.0.1:9090"
"#;
        let config: AppConfig = toml::from_str(toml).expect("zero parses");
        assert_eq!(config.runtime.max_batches_per_run, 0);
    }
}
