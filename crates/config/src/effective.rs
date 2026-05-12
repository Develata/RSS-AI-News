use std::num::NonZeroU32;

use rss_ai_news_domain::Score0To100;

use crate::{CategoryConfig, LoadedConfig};

pub struct EffectiveConfig<'a> {
    pub category: &'a CategoryConfig,
    pub ai_enabled: bool,
    pub include_unscored: bool,
    pub max_items_per_report: NonZeroU32,
    pub min_importance_score: Score0To100,
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

        // Per docs/design/config-schema.md §4.5 (lines 221-225), effective values are
        // computed as `category.publish_override.X.unwrap_or(app.publish.X)`. The
        // global defaults live in [publish] section of app.toml; per-category overrides
        // are field-level (a missing override field inherits the global value).
        Some(EffectiveConfig {
            category,
            ai_enabled: self.app.ai.enabled,
            include_unscored: publish_override
                .and_then(|override_| override_.include_unscored)
                .unwrap_or(self.app.publish.include_unscored),
            max_items_per_report: publish_override
                .and_then(|override_| override_.max_items_per_report)
                .unwrap_or(self.app.publish.max_items_per_report),
            // W2-B-1: PublishOverride.min_importance_score is now
            // Option<Score0To100> (see crates/config/src/category.rs +
            // docs/design/config-schema.md §8 line 378). Out-of-range TOML
            // values are rejected at deserialization, so this fold is a
            // straight `unwrap_or(global_default)` — the previous
            // `Score0To100::try_new(...).ok()` step (which silently masked
            // invalid configs into the default) is gone.
            min_importance_score: publish_override
                .and_then(|override_| override_.min_importance_score)
                .unwrap_or(self.app.publish.min_importance_score),
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
    use std::num::NonZeroU32;

    use rss_ai_news_domain::Score0To100;

    use crate::{
        app::{
            AiConfig, AiRateLimitConfig, AppConfig, ArtifactConfig, DatabaseConfig, DatabaseDriver,
            DedupConfig, ExtractorConfig, HttpConfig, LeaseConfig, ObservabilityConfig,
            PublishConfig, RetentionPolicy, RetryConfig, RuntimeConfig,
        },
        category::{AiOverride, CategoryConfig, CategoryMeta, PublishOverride},
        env::EnvConfig,
        loader::LoadedConfig,
        overrides::CliOverrides,
    };

    /// F7-3 W3-5：把 `u8` 转 `Score0To100` 的样板抽出来。让 test 调用方
    /// 写 `score(75)` 而非 `Score0To100::try_new(75).expect(...)`，同时若
    /// 误写 `>100` 的常量，panic 发生在该调用点（locality of error）。
    fn score(v: u8) -> Score0To100 {
        Score0To100::try_new(v).expect("test fixture: value must be 0..=100")
    }

    fn loaded(
        include_unscored: bool,
        category_override: Option<bool>,
        model: Option<&str>,
    ) -> LoadedConfig {
        loaded_with_publish_globals(
            include_unscored,
            category_override,
            model,
            30,
            30,
            None,
            None,
        )
    }

    /// F7-3 W3-5 修复：`override_min_score` 参数从 `Option<u8>` 收紧为
    /// `Option<Score0To100>`，避免历史上"测试传 200 → 在本函数内 expect
    /// panic"的低位错误信号。强类型把"必须 0..=100"的约束推回到调用方
    /// 调用 [`score`] 的位置。生产路径 [`PublishOverride::min_importance_score`]
    /// 早在 F5-4 已经升为 `Option<Score0To100>`（详见 config-schema §8
    /// line 378），本函数的签名漂移是 F5-4 的遗留。
    fn loaded_with_publish_globals(
        include_unscored: bool,
        category_override: Option<bool>,
        model: Option<&str>,
        global_max_items: u32,
        global_min_score: u8,
        override_max_items: Option<u32>,
        override_min_score: Option<Score0To100>,
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
                    max_items_per_report: NonZeroU32::new(global_max_items)
                        .expect("test: global_max_items must be non-zero"),
                    min_importance_score: Score0To100::try_new(global_min_score)
                        .expect("test: global_min_score must be 0..=100"),
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
                    max_items_per_report: override_max_items
                        .map(|v| NonZeroU32::new(v).expect("test: override_max_items non-zero")),
                    min_importance_score: override_min_score,
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

    #[test]
    fn max_items_inherits_global_when_override_absent() {
        // Per docs/design/config-schema.md §4.5 (lines 221-222):
        // effective.max_items_per_report =
        //   category.publish_override.max_items_per_report.unwrap_or(publish.max_items_per_report)
        let config = loaded_with_publish_globals(false, None, None, 30, 30, None, None);
        let effective = config.effective_for_category("ai").unwrap();
        assert_eq!(effective.max_items_per_report.get(), 30);
    }

    #[test]
    fn max_items_override_takes_precedence_over_global() {
        let config = loaded_with_publish_globals(false, None, None, 30, 30, Some(7), None);
        let effective = config.effective_for_category("ai").unwrap();
        assert_eq!(effective.max_items_per_report.get(), 7);
    }

    #[test]
    fn min_score_inherits_global_when_override_absent() {
        // Default global is 30 per the W0 freeze contract; with no override,
        // effective must equal global. Regression guard for the F4 audit
        // finding that ai_run.rs once hardcoded `unwrap_or(50)`.
        let config = loaded_with_publish_globals(false, None, None, 30, 30, None, None);
        let effective = config.effective_for_category("ai").unwrap();
        assert_eq!(effective.min_importance_score.get(), 30);
    }

    #[test]
    fn min_score_override_takes_precedence_over_global() {
        let config = loaded_with_publish_globals(false, None, None, 30, 30, None, Some(score(75)));
        let effective = config.effective_for_category("ai").unwrap();
        assert_eq!(effective.min_importance_score.get(), 75);
    }

    #[test]
    fn min_score_override_zero_is_explicit_no_floor_not_default() {
        // Per config-schema.md §4.5: `min_importance_score = 0` is "explicit
        // no floor" and must NOT be reinterpreted as "use global default".
        let config = loaded_with_publish_globals(false, None, None, 30, 30, None, Some(score(0)));
        let effective = config.effective_for_category("ai").unwrap();
        assert_eq!(effective.min_importance_score.get(), 0);
    }
}
