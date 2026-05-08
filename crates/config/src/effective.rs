use std::num::NonZeroU32;

use crate::{CategoryConfig, LoadedConfig};

pub const DEFAULT_MAX_ITEMS_PER_REPORT: NonZeroU32 = match NonZeroU32::new(30) {
    Some(v) => v,
    None => panic!("default max_items_per_report must be non-zero"),
};
pub const DEFAULT_MIN_IMPORTANCE_SCORE: u8 = 30;

pub struct EffectiveConfig<'a> {
    pub category: &'a CategoryConfig,
    pub ai_enabled: bool,
    pub include_unscored: bool,
    pub max_items_per_report: NonZeroU32,
    pub min_importance_score: u8,
    pub model: String,
    pub max_input_chars: u32,
    /// Empty when the category does not provide a prompt; runtime decides fallback behavior.
    pub prompt_template: String,
}

impl LoadedConfig {
    pub fn effective_for_category(&self, category_key: &str) -> Option<EffectiveConfig<'_>> {
        let category = self
            .categories
            .iter()
            .find(|category| category.category.key == category_key)?;
        let ai_override = category.ai_override.as_ref();
        let publish_override = category.publish_override.as_ref();

        // app.toml has no global max_items_per_report or min_importance_score. The W3
        // config layer uses conservative defaults when a category omits these fields.
        Some(EffectiveConfig {
            category,
            ai_enabled: self.app.ai.enabled,
            include_unscored: publish_override
                .and_then(|override_| override_.include_unscored)
                .unwrap_or(self.app.publish.include_unscored),
            max_items_per_report: publish_override
                .and_then(|override_| override_.max_items_per_report)
                .unwrap_or(DEFAULT_MAX_ITEMS_PER_REPORT),
            min_importance_score: publish_override
                .and_then(|override_| override_.min_importance_score)
                .unwrap_or(DEFAULT_MIN_IMPORTANCE_SCORE),
            model: ai_override
                .and_then(|override_| override_.model.as_ref())
                .filter(|model| !model.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| self.app.ai.model.clone()),
            max_input_chars: ai_override
                .and_then(|override_| override_.max_input_chars)
                .unwrap_or(self.app.ai.max_input_chars),
            prompt_template: ai_override
                .and_then(|override_| override_.prompt_template.clone())
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{
            AiConfig, AiRateLimitConfig, AppConfig, ArtifactConfig, DatabaseConfig, DatabaseDriver,
            DedupConfig, ExtractorConfig, HttpConfig, LeaseConfig, ObservabilityConfig,
            PublishConfig, RetentionPolicy, RetryConfig,
        },
        category::{AiOverride, CategoryConfig, CategoryMeta, PublishOverride},
        env::EnvConfig,
        loader::LoadedConfig,
        overrides::CliOverrides,
    };

    fn loaded(
        include_unscored: bool,
        category_override: Option<bool>,
        model: Option<&str>,
    ) -> LoadedConfig {
        LoadedConfig {
            env: EnvConfig::default(),
            app: AppConfig {
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
                    target_timezone: "Asia/Shanghai".to_string(),
                    github_owner: String::new(),
                    github_repo: String::new(),
                    github_branch: "main".to_string(),
                    github_path_prefix: "archive".to_string(),
                    local_output_dir: "output".into(),
                    include_unscored,
                },
                dedup: DedupConfig {
                    enable_link_dedup: true,
                    enable_content_dedup: true,
                    link_normalizer_version: "1".to_string(),
                },
                extractor: ExtractorConfig {
                    strategy_order: vec!["readability".to_string()],
                    max_body_bytes: 1024,
                    min_body_chars: 1,
                },
                lease: LeaseConfig {
                    fetch_duration_seconds: 1,
                    ai_duration_seconds: 1,
                    publish_duration_seconds: 1,
                    reclaim_interval_seconds: 1,
                },
                retry: RetryConfig {
                    feed_entry_max_attempts: 1,
                    ai_max_attempts: 1,
                    publish_max_attempts: 1,
                },
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
            },
            categories: vec![CategoryConfig {
                schema_version: "1".to_string(),
                category: CategoryMeta {
                    key: "ai".to_string(),
                    display_name: "AI".to_string(),
                    priority: 10,
                },
                ai_override: Some(AiOverride {
                    prompt_template: None,
                    max_input_chars: None,
                    model: model.map(str::to_string),
                }),
                publish_override: Some(PublishOverride {
                    max_items_per_report: None,
                    min_importance_score: None,
                    include_unscored: category_override,
                }),
                sources: vec![],
            }],
            config_sha256: String::new(),
            cli_overrides: CliOverrides::default(),
        }
    }

    #[test]
    fn include_unscored_uses_category_override_when_present() {
        assert!(
            loaded(false, Some(true), None)
                .effective_for_category("ai")
                .unwrap()
                .include_unscored
        );
    }

    #[test]
    fn include_unscored_inherits_global_true() {
        assert!(
            loaded(true, None, None)
                .effective_for_category("ai")
                .unwrap()
                .include_unscored
        );
    }

    #[test]
    fn include_unscored_inherits_global_false() {
        assert!(
            !loaded(false, None, None)
                .effective_for_category("ai")
                .unwrap()
                .include_unscored
        );
    }

    #[test]
    fn empty_model_inherits_global_model() {
        assert_eq!(
            loaded(false, None, Some(""))
                .effective_for_category("ai")
                .unwrap()
                .model,
            "gpt-4o-mini"
        );
    }

    #[test]
    fn non_empty_model_overrides_global_model() {
        assert_eq!(
            loaded(false, None, Some("claude"))
                .effective_for_category("ai")
                .unwrap()
                .model,
            "claude"
        );
    }

    #[test]
    fn ai_enabled_is_always_global_and_include_unscored_remains_effective_setting() {
        let mut config = loaded(true, None, None);
        config.app.ai.enabled = false;
        let effective = config.effective_for_category("ai").unwrap();

        assert!(!effective.ai_enabled);
        assert!(effective.include_unscored);
    }
}
