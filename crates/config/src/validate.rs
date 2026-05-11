mod checks;

use checks::{collect_env_checks, collect_general_checks, collect_publish_checks};

use crate::{AppConfig, CategoryConfig, ConfigError, DiagnosticReport, EnvConfig, LoadedConfig};

pub const SUPPORTED_SCHEMA_VERSION: &str = "1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Publish,
    AiRun,
    Doctor,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommandFlags {
    pub local_only: bool,
}

pub fn run_general_checks(
    app: &AppConfig,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) -> Result<(), ConfigError> {
    let mut report = DiagnosticReport::new(Vec::new());
    collect_general_checks(&mut report, app, categories, env);
    collect_env_checks(&mut report, app, categories, env);
    fail_if_needed(report)
}

/// Structural checks only: schema_version, category/source uniqueness, URL
/// well-formedness, app value ranges. Skips env-credential presence checks
/// so infrastructure commands (migrate) can run without OPENAI_* / RSSHUB_BASE_URL.
pub fn run_structural_checks(
    app: &AppConfig,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) -> Result<(), ConfigError> {
    let mut report = DiagnosticReport::new(Vec::new());
    collect_general_checks(&mut report, app, categories, env);
    fail_if_needed(report)
}

pub fn run_env_checks(
    app: &AppConfig,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) -> Result<(), ConfigError> {
    let mut report = DiagnosticReport::new(Vec::new());
    collect_env_checks(&mut report, app, categories, env);
    fail_if_needed(report)
}

pub fn run_command_checks(
    config: &LoadedConfig,
    command: CommandKind,
    flags: &CommandFlags,
) -> Result<(), ConfigError> {
    match command {
        CommandKind::Publish => {
            let mut report = DiagnosticReport::new(Vec::new());
            collect_publish_checks(&mut report, config, flags);
            fail_if_needed(report)
        }
        CommandKind::AiRun => {
            if !config.app.ai.enabled {
                return Err(ConfigError::AiRunWhileDisabled);
            }
            run_env_checks(&config.app, &config.categories, &config.env)
        }
        CommandKind::Doctor => {
            // doctor 只校验"配置本身是否有效"，不把"某命令与当前配置不兼容"
            // 视为错误。ai.enabled=false 是合法状态，是否允许 ai-run 由 AiRun
            // 分支在调用时单独判断，不应让 doctor 整体失败。
            let mut report = DiagnosticReport::new(Vec::new());
            collect_general_checks(&mut report, &config.app, &config.categories, &config.env);
            collect_env_checks(&mut report, &config.app, &config.categories, &config.env);
            collect_publish_checks(&mut report, config, flags);
            fail_if_needed(report)
        }
        CommandKind::Other => Ok(()),
    }
}

fn fail_if_needed(report: DiagnosticReport) -> Result<(), ConfigError> {
    if report.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::ValidationFailed { report })
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::{
        app::*,
        category::{CategoryMeta, SourceConfig},
    };
    use rss_ai_news_domain::Score0To100;
    use rss_ai_news_domain::state::FeedKind;

    fn app(ai_enabled: bool) -> AppConfig {
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
                enabled: ai_enabled,
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
                include_unscored: false,
                max_items_per_report: NonZeroU32::new(30).expect("test default non-zero"),
                min_importance_score: Score0To100::try_new(30).expect("test default in range"),
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
        }
    }

    fn category(key: &str, feed_url: &str) -> CategoryConfig {
        CategoryConfig {
            schema_version: "1".to_string(),
            category: CategoryMeta {
                key: key.to_string(),
                display_name: key.to_string(),
                priority: 10,
            },
            ai_override: None,
            publish_override: None,
            sources: vec![SourceConfig {
                key: "source".to_string(),
                display_name: "Source".to_string(),
                feed_url: feed_url.to_string(),
                feed_kind: FeedKind::Rss,
                priority: 10,
                enabled: true,
            }],
        }
    }

    fn env_with_ai() -> EnvConfig {
        EnvConfig {
            openai_api_key: Some(rss_ai_news_domain::SecretString::new("sk-test")),
            openai_base_url: Some("https://api.example.test/v1".to_string()),
            ..EnvConfig::default()
        }
    }

    fn loaded(app: AppConfig, categories: Vec<CategoryConfig>, env: EnvConfig) -> LoadedConfig {
        LoadedConfig {
            env,
            app,
            categories,
            config_sha256: String::new(),
            cli_overrides: crate::CliOverrides::default(),
        }
    }

    #[test]
    fn missing_openai_key_with_ai_enabled_fails() {
        let err = run_general_checks(
            &app(true),
            &[category("ai", "https://example.test/feed")],
            &EnvConfig::default(),
        )
        .expect_err("missing OpenAI env fails");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn missing_openai_key_with_ai_disabled_is_ok() {
        run_general_checks(
            &app(false),
            &[category("ai", "https://example.test/feed")],
            &EnvConfig::default(),
        )
        .expect("OpenAI env is optional when AI disabled");
    }

    #[test]
    fn missing_github_token_for_remote_publish_fails() {
        let mut app = app(false);
        app.publish.github_owner = "owner".to_string();
        app.publish.github_repo = "repo".to_string();
        let config = loaded(app, vec![], EnvConfig::default());

        let err = run_command_checks(&config, CommandKind::Publish, &CommandFlags::default())
            .expect_err("missing token fails");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn missing_github_token_with_local_only_is_ok() {
        let mut app = app(false);
        app.publish.github_owner = "owner".to_string();
        app.publish.github_repo = "repo".to_string();
        let config = loaded(app, vec![], EnvConfig::default());

        run_command_checks(
            &config,
            CommandKind::Publish,
            &CommandFlags { local_only: true },
        )
        .expect("local-only skips GitHub token");
    }

    #[test]
    fn ai_run_with_ai_disabled_returns_specific_error() {
        let config = loaded(app(false), vec![], EnvConfig::default());
        let err = run_command_checks(&config, CommandKind::AiRun, &CommandFlags::default())
            .expect_err("ai-run disabled fails");
        assert!(matches!(err, ConfigError::AiRunWhileDisabled));
    }

    #[test]
    fn rsshub_placeholder_without_base_url_fails() {
        let err = run_general_checks(
            &app(false),
            &[category("ai", "{RSSHUB}/feed")],
            &EnvConfig::default(),
        )
        .expect_err("missing RSSHub base fails");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn duplicate_category_key_fails() {
        let categories = vec![
            category("ai", "https://example.test/1"),
            category("ai", "https://example.test/2"),
        ];
        let err = run_general_checks(&app(false), &categories, &EnvConfig::default())
            .expect_err("duplicate category fails");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn invalid_feed_url_fails() {
        let err = run_general_checks(
            &app(false),
            &[category("ai", "not a url")],
            &EnvConfig::default(),
        )
        .expect_err("invalid URL fails");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn unsupported_schema_version_fails() {
        let mut app = app(false);
        app.schema_version = "2".to_string();
        let err = run_general_checks(
            &app,
            &[category("ai", "https://example.test/feed")],
            &EnvConfig::default(),
        )
        .expect_err("unsupported schema fails");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn structural_checks_ignore_missing_openai_env_when_ai_enabled() {
        run_structural_checks(
            &app(true),
            &[category("ai", "https://example.test/feed")],
            &EnvConfig::default(),
        )
        .expect("structural checks ignore missing OPENAI_* when ai enabled");
    }

    #[test]
    fn structural_checks_ignore_missing_rsshub_env_when_placeholder_used() {
        run_structural_checks(
            &app(false),
            &[category("ai", "{RSSHUB}/feed")],
            &EnvConfig::default(),
        )
        .expect("structural checks ignore missing RSSHUB_BASE_URL placeholder");
    }

    #[test]
    fn structural_checks_still_fail_on_unsupported_schema() {
        let mut app = app(false);
        app.schema_version = "2".to_string();
        let err = run_structural_checks(
            &app,
            &[category("ai", "https://example.test/feed")],
            &EnvConfig::default(),
        )
        .expect_err("structural checks still enforce schema_version");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn tokens_per_minute_zero_is_valid() {
        let mut env = env_with_ai();
        env.rsshub_base_url = Some("https://rsshub.example.test".to_string());
        run_general_checks(
            &app(true),
            &[category("ai", "https://example.test/feed")],
            &env,
        )
        .expect("tokens_per_minute=0 is valid");
    }
}
