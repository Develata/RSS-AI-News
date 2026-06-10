//! W14-B 板块 AI 凭证折叠 — 单一真相源。
//!
//! `base_url` 与 `api_key_env` 的继承相互独立（板块可只换 key 不换 endpoint）。
//! 解析失败的错误消息只含 **env 变量名**，绝不含值（SecretString 契约）。
//! 契约见 docs/plan/14-ai-fallback.md §B.3。

use rss_ai_news_domain::SecretString;

use crate::{CategoryConfig, ConfigError, Diagnostic, DiagnosticReport, EnvConfig, LoadedConfig};

/// 按板块折叠后的 AI 凭证。composition root 据此构造单 client。
#[derive(Clone, Debug)]
pub struct AiCredentials {
    pub base_url: String,
    pub api_key: SecretString,
}

impl LoadedConfig {
    /// 解析指定板块的有效 AI 凭证（override 非空 > 全局 env）。
    ///
    /// 失败返回 `ConfigError::ValidationFailed`，诊断含缺失的 env 变量名
    /// 与板块来源文件，供 ai-run 选定板块后 fail-fast。
    pub fn ai_credentials_for_category(
        &self,
        category_key: &str,
    ) -> Result<AiCredentials, ConfigError> {
        let Some(category) = self
            .categories
            .iter()
            .find(|category| category.category.key == category_key)
        else {
            return Err(ConfigError::ValidationFailed {
                report: DiagnosticReport::new(vec![Diagnostic::new(
                    "categories/",
                    "category.key",
                    format!("category {category_key:?} not found in loaded config"),
                )]),
            });
        };
        resolve_ai_credentials(category, &self.env)
    }
}

/// 对每个板块复用同一折叠逻辑做全量审计（`validate-config` 用）。
/// 任一板块凭证不可解析即聚合为一份 `ValidationFailed` 报告。
pub fn audit_ai_credentials(loaded: &LoadedConfig) -> Result<(), ConfigError> {
    if !loaded.app.ai.enabled {
        return Ok(());
    }
    let mut report = DiagnosticReport::new(Vec::new());
    for category in &loaded.categories {
        match resolve_ai_credentials(category, &loaded.env) {
            Ok(_) => {}
            Err(ConfigError::ValidationFailed { report: failure }) => report.extend(failure),
            // resolve_ai_credentials 当前只产生 ValidationFailed；其它变体
            // 属未预期错误，直接上抛而非静默聚合。
            Err(other) => return Err(other),
        }
    }
    if report.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::ValidationFailed { report })
    }
}

/// 折叠核心（纯函数，不依赖 `AppConfig`）：
/// - `base_url`：override 非空（trim）> `OPENAI_BASE_URL`
/// - `api_key`：`api_key_env` 非空 → `resolve_secret(名)`，否则 `OPENAI_API_KEY`
fn resolve_ai_credentials(
    category: &CategoryConfig,
    env: &EnvConfig,
) -> Result<AiCredentials, ConfigError> {
    let source_file = format!("categories/{}.toml", category.category.key);
    let mut report = DiagnosticReport::new(Vec::new());

    let base_url = match category.ai_base_url() {
        Some(override_url) => Some(override_url.to_string()),
        None => match env
            .openai_base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(global) => Some(global.to_string()),
            None => {
                report.push(Diagnostic::new(
                    source_file.clone(),
                    "category.ai_override.base_url",
                    format!(
                        "category {:?} inherits global base_url but OPENAI_BASE_URL is not set",
                        category.category.key
                    ),
                ));
                None
            }
        },
    };

    let api_key = match category.ai_api_key_env() {
        Some(env_name) => match env.resolve_secret(env_name) {
            Some(secret) => Some(secret),
            None => {
                report.push(Diagnostic::new(
                    source_file.clone(),
                    "category.ai_override.api_key_env",
                    format!(
                        "env variable {env_name:?} (referenced by category {:?}) is not set or blank",
                        category.category.key
                    ),
                ));
                None
            }
        },
        None => match env.openai_api_key.clone() {
            Some(secret) => Some(secret),
            None => {
                report.push(Diagnostic::new(
                    source_file,
                    "category.ai_override.api_key_env",
                    format!(
                        "category {:?} inherits global api key but OPENAI_API_KEY is not set",
                        category.category.key
                    ),
                ));
                None
            }
        },
    };

    match (base_url, api_key) {
        (Some(base_url), Some(api_key)) if report.is_empty() => {
            Ok(AiCredentials { base_url, api_key })
        }
        _ => Err(ConfigError::ValidationFailed { report }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::category::{AiOverride, CategoryMeta};

    fn category(key: &str, base_url: Option<&str>, api_key_env: Option<&str>) -> CategoryConfig {
        CategoryConfig {
            schema_version: "1".to_string(),
            category: CategoryMeta {
                key: key.to_string(),
                display_name: key.to_string(),
                priority: 10,
            },
            ai_override: Some(AiOverride {
                base_url: base_url.map(str::to_string),
                api_key_env: api_key_env.map(str::to_string),
                ..AiOverride::default()
            }),
            publish_override: None,
            sources: vec![],
        }
    }

    fn env_with(values: Vec<(&str, &str)>) -> EnvConfig {
        let mut env = EnvConfig::with_file_values(
            values
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        );
        // 固定字段不会从 with_file_values 自动抽取（绕过 load()），按需补齐。
        env.openai_base_url = values
            .iter()
            .find(|(name, _)| *name == "OPENAI_BASE_URL")
            .map(|(_, value)| value.to_string());
        env.openai_api_key = values
            .iter()
            .find(|(name, _)| *name == "OPENAI_API_KEY")
            .map(|(_, value)| SecretString::new(*value));
        env
    }

    #[test]
    fn category_overrides_take_precedence_over_global() {
        let env = env_with(vec![
            ("OPENAI_BASE_URL", "https://global.test/v1"),
            ("OPENAI_API_KEY", "sk-global"),
            ("DEEPSEEK_API_KEY", "sk-deepseek"),
        ]);
        let category = category(
            "ai",
            Some("https://api.deepseek.com/v1"),
            Some("DEEPSEEK_API_KEY"),
        );

        let creds = resolve_ai_credentials(&category, &env).expect("resolves");
        assert_eq!(creds.base_url, "https://api.deepseek.com/v1");
        assert_eq!(creds.api_key.expose_secret(), "sk-deepseek");
    }

    #[test]
    fn blank_overrides_inherit_global() {
        let env = env_with(vec![
            ("OPENAI_BASE_URL", "https://global.test/v1"),
            ("OPENAI_API_KEY", "sk-global"),
        ]);
        // 空串（trim）= 继承，与 model 同语义。
        let category = category("ai", Some("  "), Some(""));

        let creds = resolve_ai_credentials(&category, &env).expect("resolves");
        assert_eq!(creds.base_url, "https://global.test/v1");
        assert_eq!(creds.api_key.expose_secret(), "sk-global");
    }

    #[test]
    fn key_and_base_inherit_independently() {
        let env = env_with(vec![
            ("OPENAI_BASE_URL", "https://global.test/v1"),
            ("DEEPSEEK_API_KEY", "sk-deepseek"),
        ]);
        // 只换 key 不换 endpoint：base 继承全局，key 走板块 env 引用。
        let category = category("ai", None, Some("DEEPSEEK_API_KEY"));

        let creds = resolve_ai_credentials(&category, &env).expect("resolves");
        assert_eq!(creds.base_url, "https://global.test/v1");
        assert_eq!(creds.api_key.expose_secret(), "sk-deepseek");
    }

    #[test]
    fn unresolvable_api_key_env_names_variable_not_value() {
        let env = env_with(vec![("OPENAI_BASE_URL", "https://global.test/v1")]);
        let category = category("ai", None, Some("MISSING_KEY_W14B"));

        let err = resolve_ai_credentials(&category, &env).expect_err("missing env fails");
        let message = err.to_string();
        assert!(
            message.contains("MISSING_KEY_W14B"),
            "error should name the env variable: {message}"
        );
    }

    #[test]
    fn inherited_global_key_missing_is_reported() {
        let env = env_with(vec![("OPENAI_BASE_URL", "https://global.test/v1")]);
        let category = category("ai", None, None);

        let err = resolve_ai_credentials(&category, &env).expect_err("missing global key fails");
        assert!(
            err.to_string().contains("OPENAI_API_KEY"),
            "error should name the inherited global variable: {err}"
        );
    }

    #[test]
    fn missing_base_and_key_reports_both() {
        let env = env_with(vec![]);
        let category = category("ai", None, None);

        let err = resolve_ai_credentials(&category, &env).expect_err("both missing fails");
        match err {
            ConfigError::ValidationFailed { report } => {
                assert_eq!(report.diagnostics.len(), 2, "{report}");
            }
            other => panic!("expected ValidationFailed, got {other}"),
        }
    }
}
