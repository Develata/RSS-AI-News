use std::path::PathBuf;

use crate::AppConfig;

#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub db_path: Option<PathBuf>,
    pub log_level: Option<String>,
    pub log_format: Option<String>,
    pub timezone: Option<String>,
    pub category_filter: Option<String>,
    pub dry_run: bool,
    /// Global `--max-batches` override. `Some(n)` 落到
    /// `app.runtime.max_batches_per_run`; `None` 走 toml / 默认。
    /// 详见 [config-schema §8](docs/design/config-schema.md#8-cli-overrides-与覆盖优先级)
    /// 与 [cli-semantics §4.1/§4.2/§4.11](docs/design/cli-semantics.md)。
    pub max_batches: Option<u32>,
}

impl CliOverrides {
    pub fn apply_to_app(&self, app: &mut AppConfig) {
        if let Some(db_path) = &self.db_path {
            app.database.sqlite_path = db_path.clone();
        }
        if let Some(log_level) = &self.log_level {
            app.observability.log_level = log_level.clone();
        }
        if let Some(log_format) = &self.log_format {
            app.observability.log_format = log_format.clone();
        }
        if let Some(timezone) = &self.timezone {
            app.publish.target_timezone = timezone.clone();
        }
        if let Some(max_batches) = self.max_batches {
            app.runtime.max_batches_per_run = max_batches;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::*;
    use rss_ai_news_domain::Score0To100;
    use std::num::NonZeroU32;

    fn baseline_app() -> AppConfig {
        AppConfig {
            schema_version: "1".to_string(),
            database: DatabaseConfig {
                driver: DatabaseDriver::Sqlite,
                sqlite_path: "data.db".into(),
                max_connections: 5,
                busy_timeout_ms: 5000,
            },
            http: HttpConfig {
                user_agent: "test".to_string(),
                timeout_seconds: 30,
                max_retries: 3,
                retry_backoff_base_ms: 1000,
                concurrent_feeds: 10,
                concurrent_fetches: 5,
            },
            ai: AiConfig {
                enabled: true,
                model: "gpt-4o-mini".to_string(),
                fallback_models: Vec::new(),
                max_tokens: 4096,
                temperature: 0.3,
                request_timeout_seconds: 60,
                max_input_chars: 8000,
                rate_limit: AiRateLimitConfig {
                    requests_per_minute: 60,
                    tokens_per_minute: 0,
                },
            },
            publish: PublishConfig {
                target_timezone: "UTC".to_string(),
                github_owner: String::new(),
                github_repo: String::new(),
                github_branch: "main".to_string(),
                github_path_prefix: "archive".to_string(),
                local_output_dir: "output".into(),
                template: PublishTemplateConfig::default(),
                include_unscored: false,
                max_items_per_report: NonZeroU32::new(30).unwrap(),
                min_importance_score: Score0To100::try_new(30).unwrap(),
                candidate_window_hours: 48,
            },
            dedup: DedupConfig {
                enable_link_dedup: true,
                enable_content_dedup: true,
                link_normalizer_version: "1".to_string(),
            },
            extractor: ExtractorConfig {
                strategy_order: vec!["readability".to_string()],
                max_body_bytes: 1024,
                min_body_chars: 100,
            },
            lease: LeaseConfig {
                fetch_duration_seconds: 1,
                ai_duration_seconds: 1,
                publish_duration_seconds: 1,
                reclaim_interval_seconds: 1,
            },
            retry: RetryConfig {
                feed_entry_max_attempts: 5,
                ai_max_attempts: 3,
                publish_max_attempts: 5,
            },
            runtime: RuntimeConfig::default(),
            artifact: ArtifactConfig {
                retention_policy: RetentionPolicy::OnFailure,
                sample_rate: 0.1,
                inline_threshold_bytes: 1024,
                file_storage_dir: "artifacts".into(),
                ttl_days: 30,
            },
            observability: ObservabilityConfig {
                log_level: "info".to_string(),
                log_format: "pretty".to_string(),
                log_file: String::new(),
                enable_metrics: false,
                metrics_bind: "127.0.0.1:9090".to_string(),
            },
        }
    }

    #[test]
    fn max_batches_some_overrides_runtime_config() {
        // §8 line 405: `--max-batches <n>` 覆盖 `runtime.max_batches_per_run`。
        let overrides = CliOverrides {
            max_batches: Some(3),
            ..CliOverrides::default()
        };
        let mut app = baseline_app();
        assert_eq!(app.runtime.max_batches_per_run, 10);
        overrides.apply_to_app(&mut app);
        assert_eq!(app.runtime.max_batches_per_run, 3);
    }

    #[test]
    fn max_batches_some_zero_means_unlimited_and_passes_through() {
        // §4.4 line 196: `0` = 不限。CLI override 不应把 0 重写成默认。
        let overrides = CliOverrides {
            max_batches: Some(0),
            ..CliOverrides::default()
        };
        let mut app = baseline_app();
        overrides.apply_to_app(&mut app);
        assert_eq!(app.runtime.max_batches_per_run, 0);
    }

    #[test]
    fn max_batches_none_preserves_config_value() {
        let overrides = CliOverrides::default();
        let mut app = baseline_app();
        app.runtime.max_batches_per_run = 25;
        overrides.apply_to_app(&mut app);
        assert_eq!(app.runtime.max_batches_per_run, 25);
    }
}
