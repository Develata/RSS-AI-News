//! `[decision]` (app.toml) and `[category.decision]` (categories/*.toml).
//! The config crate passes both through as raw TOML; all meaning lives here
//! (ADR 0010 §6), so removing this crate removes the feature.

use std::{fmt, time::Duration};

use rss_ai_news_domain::SecretString;
use serde::Deserialize;
use url::Url;

use crate::{
    client::{ClientSettings, SystemOneClient, check_base_url},
    error::DecisionError,
    provider::{self, Provider},
    question::{ChoiceOption, Issue, QuestionKind, QuestionSpec, validate_common},
};

pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 10;
pub const DEFAULT_MAX_STATE_CHARS: u32 = 6000;
pub const DEFAULT_LAYA_MAX_LEN: u32 = 2048;

#[derive(Clone, PartialEq, Eq)]
pub struct DecisionSettings {
    pub enabled: bool,
    pub provider: Provider,
    pub base_url: Option<Url>,
    /// Name of the env variable holding the key; the key never sits in TOML.
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    pub request_timeout: Duration,
    pub max_state_chars: u32,
    pub laya_max_len: u32,
    laya_max_len_set: bool,
}

/// Origin only for `base_url`; the key's variable name is not a secret.
impl fmt::Debug for DecisionSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionSettings")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field(
                "origin",
                &self
                    .base_url
                    .as_ref()
                    .map(|url| url.origin().ascii_serialization()),
            )
            .field("api_key_env", &self.api_key_env)
            .field("model", &self.model)
            .field("request_timeout", &self.request_timeout)
            .field("max_state_chars", &self.max_state_chars)
            .field("laya_max_len", &self.laya_max_len)
            .finish_non_exhaustive()
    }
}

/// Non-fatal, value-free warning; the CLI renders it like config warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: &'static str,
    pub field: &'static str,
    pub message: &'static str,
    pub category: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionIssue {
    /// `app.toml` or `categories/<file>`.
    pub source: String,
    pub field: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub issues: Vec<SectionIssue>,
    pub warnings: Vec<Warning>,
}

/// One category's raw `[category.decision]` section.
#[derive(Debug, Clone, Copy)]
pub struct CategorySection<'a> {
    pub source: &'a str,
    pub category_key: &'a str,
    pub value: &'a toml::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    #[serde(default)]
    enabled: bool,
    provider: String,
    base_url: Option<String>,
    api_key_env: Option<String>,
    model: Option<String>,
    request_timeout_seconds: Option<u64>,
    max_state_chars: Option<u32>,
    laya_max_len: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCategory {
    #[serde(default)]
    questions: Vec<RawQuestion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuestion {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    instructions: String,
    yes: Option<String>,
    no: Option<String>,
    options: Option<Vec<RawOption>>,
    levels: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOption {
    label: String,
    description: Option<String>,
}

fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl DecisionSettings {
    /// Parse the raw `[decision]` table. Structural problems are issues;
    /// value-free advisories are warnings.
    pub fn from_section(value: &toml::Value) -> Result<(Self, Vec<Warning>), Vec<Issue>> {
        let raw: RawSettings = value
            .clone()
            .try_into()
            .map_err(|error: toml::de::Error| vec![Issue::new("decision", error.message())])?;
        let mut issues = Vec::new();
        let mut warnings = Vec::new();
        let provider = Provider::parse(raw.provider.trim()).unwrap_or_else(|| {
            issues.push(Issue::new(
                "decision.provider",
                "must be \"laya\" or \"jev\"",
            ));
            Provider::Laya
        });
        let base_url = non_blank(raw.base_url).and_then(|text| {
            let checked = Url::parse(&text)
                .map_err(|_| "must be an http(s) URL with a host")
                .and_then(|url| check_base_url(&url).map(|()| url));
            checked
                .map_err(|message| issues.push(Issue::new("decision.base_url", message)))
                .ok()
        });
        let api_key_env = non_blank(raw.api_key_env);
        if let Some(name) = &api_key_env
            && !name
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            issues.push(Issue::new(
                "decision.api_key_env",
                "must be an env variable name ([A-Za-z0-9_])",
            ));
        }
        let model = non_blank(raw.model);
        if let Some(model) = &model {
            match provider::check_model(provider, model) {
                Ok(true) => warnings.push(Warning {
                    code: "decision_model_alias",
                    field: "decision.model",
                    message: "Moving Jev alias: answers can change without a config change; pin a version such as jev-1.13.0.",
                    category: None,
                }),
                Ok(false) => {}
                Err(message) => issues.push(Issue::new("decision.model", message)),
            }
        }
        let timeout = raw
            .request_timeout_seconds
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECONDS);
        if !(1..=60).contains(&timeout) {
            issues.push(Issue::new(
                "decision.request_timeout_seconds",
                "must be in 1..=60",
            ));
        }
        let max_state_chars = raw.max_state_chars.unwrap_or(DEFAULT_MAX_STATE_CHARS);
        if !(1..=200_000).contains(&max_state_chars) {
            issues.push(Issue::new(
                "decision.max_state_chars",
                "must be in 1..=200000",
            ));
        }
        let laya_max_len = raw.laya_max_len.unwrap_or(DEFAULT_LAYA_MAX_LEN);
        if !provider::LAYA_MAX_LEN.contains(&laya_max_len) {
            issues.push(Issue::new("decision.laya_max_len", "must be in 512..=8192"));
        }
        if provider == Provider::Jev && raw.laya_max_len.is_some() {
            warnings.push(Warning {
                code: "decision_unused_field",
                field: "decision.laya_max_len",
                message: "Ignored for provider = \"jev\".",
                category: None,
            });
        }
        if raw.enabled {
            for (field, present) in [
                ("decision.base_url", base_url.is_some()),
                ("decision.api_key_env", api_key_env.is_some()),
                ("decision.model", model.is_some()),
            ] {
                if !present {
                    issues.push(Issue::new(field, "required when enabled = true"));
                }
            }
        }
        if !issues.is_empty() {
            return Err(issues);
        }
        Ok((
            Self {
                enabled: raw.enabled,
                provider,
                base_url,
                api_key_env,
                model,
                request_timeout: Duration::from_secs(timeout),
                max_state_chars,
                laya_max_len,
                laya_max_len_set: raw.laya_max_len.is_some(),
            },
            warnings,
        ))
    }

    /// The client when enabled; `Ok(None)` when disabled, without touching
    /// `resolve_key` (no credential is looked up, no client is built).
    pub fn build_client(
        &self,
        resolve_key: impl FnOnce(&str) -> Option<SecretString>,
    ) -> Result<Option<SystemOneClient>, DecisionError> {
        if !self.enabled {
            return Ok(None);
        }
        let missing = |field: &str| DecisionError::InvalidConfig(format!("{field} is required"));
        let base_url = self.base_url.clone().ok_or_else(|| missing("base_url"))?;
        let name = self
            .api_key_env
            .as_deref()
            .ok_or_else(|| missing("api_key_env"))?;
        let model = self.model.clone().ok_or_else(|| missing("model"))?;
        let key = resolve_key(name).ok_or_else(|| {
            DecisionError::InvalidConfig(format!("env variable {name} is not set or blank"))
        })?;
        SystemOneClient::new(
            ClientSettings {
                provider: self.provider,
                base_url,
                model,
                request_timeout: self.request_timeout,
                max_state_chars: self.max_state_chars,
                laya_max_len: self.laya_max_len,
                use_env_proxy: true,
            },
            key,
        )
        .map(Some)
    }

    pub fn laya_max_len_is_set(&self) -> bool {
        self.laya_max_len_set
    }
}

/// Parse a `[category.decision]` table into normalised questions.
/// Type-specific fields on the wrong type are issues rather than silently
/// ignored.
pub fn questions_from_section(value: &toml::Value) -> Result<Vec<QuestionSpec>, Vec<Issue>> {
    let prefix = "category.decision.questions";
    let raw: RawCategory = value
        .clone()
        .try_into()
        .map_err(|error: toml::de::Error| vec![Issue::new("category.decision", error.message())])?;
    let mut issues = Vec::new();
    let mut questions = Vec::with_capacity(raw.questions.len());
    for (index, question) in raw.questions.into_iter().enumerate() {
        let at = |field: &str| format!("{prefix}[{index}].{field}");
        let stray = |issues: &mut Vec<Issue>, present: bool, field: &str| {
            if present {
                issues.push(Issue::new(at(field), "not allowed for this question type"));
            }
        };
        let kind = match question.kind.trim() {
            "noul" => {
                stray(&mut issues, question.options.is_some(), "options");
                stray(&mut issues, question.levels.is_some(), "levels");
                Some(QuestionKind::Noul {
                    yes: question.yes,
                    no: question.no,
                })
            }
            "choice" => {
                stray(&mut issues, question.yes.is_some(), "yes");
                stray(&mut issues, question.no.is_some(), "no");
                stray(&mut issues, question.levels.is_some(), "levels");
                match question.options {
                    Some(options) => Some(QuestionKind::Choice {
                        options: options
                            .into_iter()
                            .map(|option| ChoiceOption {
                                label: option.label,
                                description: option.description,
                            })
                            .collect(),
                    }),
                    None => {
                        issues.push(Issue::new(at("options"), "required for choice"));
                        None
                    }
                }
            }
            "score" => {
                stray(&mut issues, question.yes.is_some(), "yes");
                stray(&mut issues, question.no.is_some(), "no");
                stray(&mut issues, question.options.is_some(), "options");
                match question.levels {
                    Some(levels) => Some(QuestionKind::Score { levels }),
                    None => {
                        issues.push(Issue::new(at("levels"), "required for score"));
                        None
                    }
                }
            }
            _ => {
                issues.push(Issue::new(at("type"), "must be noul, choice or score"));
                None
            }
        };
        if let Some(kind) = kind {
            let spec = QuestionSpec {
                id: question.id,
                instructions: question.instructions,
                kind,
            };
            questions.push(spec.normalized());
        }
    }
    if issues.is_empty() {
        Ok(questions)
    } else {
        Err(issues)
    }
}

/// validate-config entry point. `has_env` is `Some` only when env checks are
/// enforced; it answers whether a variable is set and non-blank, so key values
/// never reach this crate during validation.
pub fn validate_sections(
    app: Option<&toml::Value>,
    categories: &[CategorySection<'_>],
    has_env: Option<&dyn Fn(&str) -> bool>,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    let issue = |source: &str, issue: Issue| SectionIssue {
        source: source.to_string(),
        field: issue.field,
        message: issue.message,
    };
    let settings = app.and_then(|value| match DecisionSettings::from_section(value) {
        Ok((settings, warnings)) => {
            report.warnings.extend(warnings);
            Some(settings)
        }
        Err(issues) => {
            report
                .issues
                .extend(issues.into_iter().map(|found| issue("app.toml", found)));
            None
        }
    });
    if let (Some(settings), Some(has_env)) = (&settings, has_env)
        && settings.enabled
        && let Some(name) = &settings.api_key_env
        && !has_env(name)
    {
        report.issues.push(SectionIssue {
            source: "app.toml".to_string(),
            field: "decision.api_key_env".to_string(),
            message: format!("env variable \"{name}\" is not set or blank"),
        });
    }
    for section in categories {
        let questions = match questions_from_section(section.value) {
            Ok(questions) => questions,
            Err(issues) => {
                report
                    .issues
                    .extend(issues.into_iter().map(|found| issue(section.source, found)));
                continue;
            }
        };
        let prefix = "category.decision.questions";
        let issues = match &settings {
            Some(settings) => provider::validate_questions(settings.provider, &questions, prefix),
            None => validate_common(&questions, prefix),
        };
        report
            .issues
            .extend(issues.into_iter().map(|found| issue(section.source, found)));
        if !questions.is_empty() && !settings.as_ref().is_some_and(|settings| settings.enabled) {
            report.warnings.push(Warning {
                code: "decision_inert",
                field: "category.decision.questions",
                message: "Questions are configured but [decision] is absent or disabled; nothing is sent.",
                category: Some(section.category_key.to_string()),
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn table(text: &str) -> toml::Value {
        toml::from_str::<toml::Table>(text).unwrap().into()
    }

    const LAYA_ENABLED: &str = r#"
enabled = true
provider = "laya"
base_url = "http://laya.internal:8080"
api_key_env = "LAYA_API_KEY"
model = "multilingual"
"#;

    const QUESTIONS: &str = r#"
[[questions]]
id = "q_marketing"
type = "noul"
instructions = " 这篇是否主要是营销稿？ "

[[questions]]
id = "q_importance"
type = "score"
instructions = "重要性？"
levels = ["琐碎", "一般", "重大"]

[[questions]]
id = "q_kind"
type = "choice"
instructions = "哪一类？"
options = [{ label = "paper", description = "论文" }, { label = "other", description = " " }]
"#;

    #[test]
    fn defaults_apply_and_disabled_needs_only_the_provider() {
        let (settings, warnings) =
            DecisionSettings::from_section(&table("provider = \"jev\"")).unwrap();
        assert!(!settings.enabled);
        assert_eq!(settings.request_timeout, Duration::from_secs(10));
        assert_eq!(settings.max_state_chars, 6000);
        assert_eq!(settings.laya_max_len, 2048);
        assert!(warnings.is_empty());
    }

    #[test]
    fn enabled_requires_endpoint_key_name_and_model() {
        let issues = DecisionSettings::from_section(&table("enabled = true\nprovider = \"laya\""))
            .unwrap_err();
        let fields: Vec<_> = issues.iter().map(|issue| issue.field.as_str()).collect();
        assert_eq!(
            fields,
            [
                "decision.base_url",
                "decision.api_key_env",
                "decision.model"
            ]
        );
    }

    #[test]
    fn base_url_must_not_carry_credentials_or_a_query() {
        for bad in [
            "http://user:pass@laya.internal",
            "http://laya.internal/?token=x",
            "ftp://laya.internal",
        ] {
            let issues = DecisionSettings::from_section(&table(&format!(
                "provider = \"laya\"\nbase_url = \"{bad}\""
            )))
            .unwrap_err();
            assert_eq!(issues[0].field, "decision.base_url", "{bad}");
        }
    }

    #[test]
    fn settings_debug_prints_the_origin_only() {
        let (settings, _) = DecisionSettings::from_section(&table(
            "provider = \"laya\"\nbase_url = \"http://laya.internal:8080/private\"",
        ))
        .unwrap();
        let debug = format!("{settings:?}");
        assert!(debug.contains("http://laya.internal:8080"));
        assert!(!debug.contains("private"));
    }

    #[test]
    fn rejects_auto_unknown_fields_and_out_of_range_values() {
        let issues = DecisionSettings::from_section(&table(
            "provider = \"laya\"\nmodel = \"auto\"\nlaya_max_len = 100\nrequest_timeout_seconds = 0",
        ))
        .unwrap_err();
        let fields: Vec<_> = issues.iter().map(|issue| issue.field.as_str()).collect();
        assert_eq!(
            fields,
            [
                "decision.model",
                "decision.request_timeout_seconds",
                "decision.laya_max_len"
            ]
        );
        assert!(DecisionSettings::from_section(&table("provider = \"laya\"\ntypo = 1")).is_err());
        assert!(DecisionSettings::from_section(&table("provider = \"openai\"")).is_err());
    }

    #[test]
    fn warns_for_moving_jev_aliases_and_unused_laya_field() {
        let (_, warnings) = DecisionSettings::from_section(&table(
            "provider = \"jev\"\nmodel = \"jev-latest\"\nlaya_max_len = 4096",
        ))
        .unwrap();
        let codes: Vec<_> = warnings.iter().map(|warning| warning.code).collect();
        assert_eq!(codes, ["decision_model_alias", "decision_unused_field"]);
    }

    #[test]
    fn questions_are_trimmed_and_blank_descriptions_dropped() {
        let questions = questions_from_section(&table(QUESTIONS)).unwrap();
        assert_eq!(questions[0].instructions, "这篇是否主要是营销稿？");
        let one_sided = questions_from_section(&table(
            "[[questions]]\nid = \"q\"\ntype = \"noul\"\ninstructions = \"x\"\nyes = \" 是 \"\nno = \" \"",
        ))
        .unwrap();
        assert_eq!(
            one_sided[0].kind,
            QuestionKind::Noul {
                yes: Some("是".to_string()),
                no: None
            }
        );
        let QuestionKind::Choice { options } = &questions[2].kind else {
            panic!("choice");
        };
        assert_eq!(options[1].description, None);
    }

    #[test]
    fn type_specific_fields_must_match_the_type() {
        let issues = questions_from_section(&table(
            "[[questions]]\nid = \"q\"\ntype = \"noul\"\ninstructions = \"x\"\nlevels = [\"a\", \"b\"]",
        ))
        .unwrap_err();
        assert_eq!(issues[0].field, "category.decision.questions[0].levels");
        assert!(
            questions_from_section(&table(
                "[[questions]]\nid = \"q\"\ntype = \"choice\"\ninstructions = \"x\""
            ))
            .is_err()
        );
    }

    #[test]
    fn disabled_build_never_resolves_the_key() {
        let mut text = LAYA_ENABLED.replace("enabled = true", "enabled = false");
        text.push('\n');
        let (settings, _) = DecisionSettings::from_section(&table(&text)).unwrap();
        let called = Cell::new(false);
        let client = settings
            .build_client(|_| {
                called.set(true);
                None
            })
            .unwrap();
        assert!(client.is_none());
        assert!(!called.get());
    }

    #[test]
    fn enabled_build_needs_the_key_and_never_reports_its_value() {
        let (settings, _) = DecisionSettings::from_section(&table(LAYA_ENABLED)).unwrap();
        let error = settings.build_client(|_| None).unwrap_err();
        assert!(error.to_string().contains("LAYA_API_KEY"));
        let client = settings
            .build_client(|_| Some(SecretString::from("sk-secret-value")))
            .unwrap()
            .unwrap();
        assert!(!format!("{client:?}").contains("sk-secret-value"));
    }

    fn sections<'a>(value: &'a toml::Value) -> [CategorySection<'a>; 1] {
        [CategorySection {
            source: "categories/ai.toml",
            category_key: "ai",
            value,
        }]
    }

    #[test]
    fn questions_without_an_enabled_section_are_inert() {
        let questions = table(QUESTIONS);
        let report = validate_sections(None, &sections(&questions), None);
        assert!(report.issues.is_empty());
        assert_eq!(report.warnings[0].code, "decision_inert");
        assert_eq!(report.warnings[0].category.as_deref(), Some("ai"));
        let disabled = table("provider = \"laya\"");
        let report = validate_sections(Some(&disabled), &sections(&questions), None);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn provider_profile_applies_and_empty_lists_are_skipped() {
        let jev = table("provider = \"jev\"");
        let eleven = table(
            "[[questions]]\nid = \"q\"\ntype = \"score\"\ninstructions = \"x\"\nlevels = [\"0\",\"1\",\"2\",\"3\",\"4\",\"5\",\"6\",\"7\",\"8\",\"9\",\"10\"]",
        );
        let report = validate_sections(Some(&jev), &sections(&eleven), None);
        assert_eq!(report.issues[0].source, "categories/ai.toml");
        assert!(report.issues[0].message.contains("at most 10"));
        let empty = table("questions = []");
        let report = validate_sections(Some(&jev), &sections(&empty), None);
        assert_eq!(report, ValidationReport::default());
    }

    #[test]
    fn env_check_runs_only_when_enabled_and_enforced() {
        let enabled = table(LAYA_ENABLED);
        let missing = |_: &str| false;
        let report = validate_sections(Some(&enabled), &[], Some(&missing));
        assert_eq!(report.issues[0].field, "decision.api_key_env");
        assert!(
            validate_sections(Some(&enabled), &[], None)
                .issues
                .is_empty()
        );
        let disabled = table("provider = \"laya\"\napi_key_env = \"LAYA_API_KEY\"");
        let called = Cell::new(false);
        let probe = |_: &str| {
            called.set(true);
            false
        };
        assert!(
            validate_sections(Some(&disabled), &[], Some(&probe))
                .issues
                .is_empty()
        );
        assert!(!called.get());
    }
}
