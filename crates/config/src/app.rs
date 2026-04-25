use std::path::PathBuf;

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
    }
}
