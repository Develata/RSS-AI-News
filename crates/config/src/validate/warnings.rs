use serde::Serialize;

use crate::{
    AppConfig, EffectiveConfig, LoadedConfig, RetentionPolicy, effective::AI_PROMPT_PLACEHOLDERS,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigWarning {
    pub code: &'static str,
    pub field: &'static str,
    pub message: &'static str,
    /// Category key for category-level warnings; absent for app-level ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// All non-fatal warnings for a loaded configuration: app-level inert fields,
/// then per-category prompt checks in category order.
pub fn config_warnings(loaded: &LoadedConfig) -> Vec<ConfigWarning> {
    let mut warnings = inert_config_warnings(&loaded.app);
    for category in &loaded.categories {
        if let Some(effective) = loaded.effective_for_category(category.key()) {
            warnings.extend(prompt_template_warnings(&effective));
        }
    }
    warnings
}

/// Compatibility fields remain accepted. Warn when they differ from the
/// shipped example baseline; never echo user-supplied paths or other values.
pub fn inert_config_warnings(app: &AppConfig) -> Vec<ConfigWarning> {
    let checks = [
        (
            app.ai.rate_limit.requests_per_minute != 60,
            "ai.rate_limit.requests_per_minute",
            "Reserved: no requests-per-minute limiter is installed; use concurrency and scheduling controls.",
        ),
        (
            app.ai.rate_limit.tokens_per_minute != 0,
            "ai.rate_limit.tokens_per_minute",
            "Reserved: no tokens-per-minute limiter is installed.",
        ),
        (
            app.http.max_retries != 3,
            "http.max_retries",
            "Ignored: HTTP clients do not use this retry count; task retries use [retry] and later runs.",
        ),
        (
            app.http.retry_backoff_base_ms != 1000,
            "http.retry_backoff_base_ms",
            "Ignored: HTTP clients do not use this backoff setting.",
        ),
        (
            !app.dedup.enable_link_dedup,
            "dedup.enable_link_dedup",
            "Ignored: database link deduplication remains enabled.",
        ),
        (
            !app.dedup.enable_content_dedup,
            "dedup.enable_content_dedup",
            "Ignored: database content deduplication remains enabled.",
        ),
        (
            app.dedup.link_normalizer_version != "1",
            "dedup.link_normalizer_version",
            "Ignored: link normalization has a single built-in version; this value selects nothing.",
        ),
        (
            app.artifact.inline_threshold_bytes != 65536,
            "artifact.inline_threshold_bytes",
            "Reserved: all artifact payloads are stored inline in the database.",
        ),
        (
            app.artifact.file_storage_dir != std::path::Path::new("data/artifacts"),
            "artifact.file_storage_dir",
            "Reserved: no artifact files are written or read from this directory.",
        ),
        (
            app.artifact.retention_policy == RetentionPolicy::DebugOnly,
            "artifact.retention_policy",
            "debug_only is not implemented and currently writes no artifacts; use always or on_failure.",
        ),
        (
            app.lease.reclaim_interval_seconds != 120,
            "lease.reclaim_interval_seconds",
            "Ignored: lease maintenance runs at flow startup, without an interval timer.",
        ),
        (
            app.observability.log_level != "info",
            "observability.log_level",
            "Ignored by CLI startup: use --log-level or RUST_LOG.",
        ),
        (
            app.observability.log_format != "pretty",
            "observability.log_format",
            "Ignored by CLI startup: use --log-format.",
        ),
        (
            !app.observability.log_file.is_empty(),
            "observability.log_file",
            "Ignored by CLI startup: use --log-file.",
        ),
        (
            app.observability.enable_metrics,
            "observability.enable_metrics",
            "Ignored by CLI startup: use --metrics-bind to start the metrics endpoint.",
        ),
        (
            app.observability.metrics_bind != "127.0.0.1:9090",
            "observability.metrics_bind",
            "Ignored by CLI startup: use --metrics-bind.",
        ),
    ];
    checks
        .into_iter()
        .filter(|(changed, _, _)| *changed)
        .map(|(_, field, message)| ConfigWarning {
            code: "inert_config",
            field,
            message,
            category: None,
        })
        .collect()
}

/// Warn when AI is enabled and the effective prompt would not send the article
/// to the model, or carries `{name}` tokens that `render_prompt` leaves
/// verbatim. Messages are fixed text; the template itself is never echoed.
pub fn prompt_template_warnings(effective: &EffectiveConfig<'_>) -> Vec<ConfigWarning> {
    if !effective.ai_enabled {
        return Vec::new();
    }
    let template = effective.prompt_template.as_str();
    let checks = [
        (
            !template.contains("{body_text}"),
            "Missing {body_text}: the article body is not sent to the model.",
        ),
        (
            !template.contains("{title}"),
            "Missing {title}: the article title is not sent to the model.",
        ),
        (
            has_unsupported_placeholder(template),
            "Unsupported {name} placeholder is sent verbatim; supported: {title}, {body_text}, {category_key}.",
        ),
    ];
    checks
        .into_iter()
        .filter(|(triggered, _)| *triggered)
        .map(|(_, message)| ConfigWarning {
            code: "prompt_template",
            field: "category.ai_override.prompt_template",
            message,
            category: Some(effective.category.key().to_string()),
        })
        .collect()
}

/// `{identifier}` that is not a supported placeholder. JSON examples such as
/// `{"summary": ...}`, `{ title }` or non-ASCII names like `{摘要}` are not
/// identifiers and stay silent. Each `{` scans only the identifier run that
/// follows it, so the check stays linear even for many unclosed braces.
fn has_unsupported_placeholder(template: &str) -> bool {
    template.match_indices('{').any(|(start, _)| {
        let rest = &template[start + 1..];
        let end = rest
            .find(|c: char| !is_identifier_char(c))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        rest[end..].starts_with('}')
            && is_identifier(name)
            && !AI_PROMPT_PLACEHOLDERS.contains(&name)
    })
}

fn is_identifier_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// ASCII `[A-Za-z_][A-Za-z0-9_]*`.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|rest| rest == '_' || rest.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> AppConfig {
        toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../configs/app.toml.example"
        )))
        .unwrap()
    }

    #[test]
    fn shipped_compatibility_baseline_is_quiet() {
        assert!(inert_config_warnings(&example()).is_empty());
    }

    #[test]
    fn warns_for_all_inert_fields_without_echoing_values() {
        let mut app = example();
        app.ai.rate_limit.requests_per_minute = 9;
        app.ai.rate_limit.tokens_per_minute = 9;
        app.http.max_retries = 9;
        app.http.retry_backoff_base_ms = 9;
        app.dedup.enable_link_dedup = false;
        app.dedup.enable_content_dedup = false;
        app.dedup.link_normalizer_version = "sensitive-fixture-value".into();
        app.artifact.inline_threshold_bytes = 9;
        app.artifact.file_storage_dir = "sensitive-fixture-value".into();
        app.artifact.retention_policy = RetentionPolicy::DebugOnly;
        app.lease.reclaim_interval_seconds = 9;
        app.observability.log_level = "debug".into();
        app.observability.log_format = "json".into();
        app.observability.log_file = "sensitive-fixture-value".into();
        app.observability.enable_metrics = true;
        app.observability.metrics_bind = "sensitive-fixture-value".into();
        let warnings = inert_config_warnings(&app);
        assert_eq!(warnings.len(), 16);
        let mut fields: Vec<_> = warnings.iter().map(|warning| warning.field).collect();
        fields.sort_unstable();
        fields.dedup();
        assert_eq!(fields.len(), 16);
        assert!(
            !serde_json::to_string(&warnings)
                .unwrap()
                .contains("sensitive-fixture-value")
        );
    }

    fn loaded_with_prompt(prompt_template: Option<&str>, ai_enabled: bool) -> LoadedConfig {
        let mut app = example();
        app.ai.enabled = ai_enabled;
        let mut category: crate::CategoryConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../configs/categories/ai.toml.example"
        )))
        .unwrap();
        if let Some(prompt_template) = prompt_template {
            category.ai_override.as_mut().unwrap().prompt_template =
                Some(prompt_template.to_string());
        }
        LoadedConfig {
            env: crate::EnvConfig::default(),
            app,
            categories: vec![category],
            source_secrets: crate::SourceSecrets::default(),
            config_sha256: String::new(),
            cli_overrides: crate::CliOverrides::default(),
        }
    }

    fn prompt_messages(loaded: &LoadedConfig) -> Vec<&'static str> {
        prompt_template_warnings(&loaded.effective_for_category("ai").unwrap())
            .into_iter()
            .map(|warning| warning.message)
            .collect()
    }

    #[test]
    fn shipped_category_example_prompt_is_quiet() {
        assert!(config_warnings(&loaded_with_prompt(None, true)).is_empty());
    }

    #[test]
    fn builtin_default_prompt_is_quiet() {
        let mut loaded = loaded_with_prompt(None, true);
        loaded.categories[0].ai_override = None;
        assert!(prompt_messages(&loaded).is_empty());
    }

    #[test]
    fn prompt_without_article_placeholders_warns_without_echoing_template() {
        let loaded = loaded_with_prompt(Some("sensitive-fixture-value: summarize"), true);
        let warnings = prompt_template_warnings(&loaded.effective_for_category("ai").unwrap());
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|warning| {
            warning.code == "prompt_template"
                && warning.field == "category.ai_override.prompt_template"
                && warning.category.as_deref() == Some("ai")
        }));
        assert!(warnings[0].message.contains("{body_text}"));
        assert!(warnings[1].message.contains("{title}"));
        assert!(
            !serde_json::to_string(&warnings)
                .unwrap()
                .contains("sensitive-fixture-value")
        );
    }

    #[test]
    fn double_brace_doc_style_template_flags_missing_body_and_unknown_placeholder() {
        // `{{title}}` still renders the title (inside literal braces), but
        // `{{body}}` is not a placeholder: the body would never be sent.
        let loaded = loaded_with_prompt(Some("标题: {{title}}\n正文:\n{{body}}"), true);
        let messages = prompt_messages(&loaded);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("Missing {body_text}"));
        assert!(messages[1].contains("Unsupported"));
    }

    #[test]
    fn json_examples_and_spaced_braces_are_not_placeholders() {
        let loaded = loaded_with_prompt(
            Some(
                "输出 {\"keep_decision\": true, \"tags\": []} 或 { title }\n\
                 {category_key} {title} {body_text}",
            ),
            true,
        );
        assert!(prompt_messages(&loaded).is_empty());
    }

    #[test]
    fn only_ascii_identifiers_closed_by_a_brace_count_as_placeholders() {
        for silent in ["{摘要}", "{9x}", "{a b}", "{a-b}", "{unclosed", "{}"] {
            let template = format!("{{title}} {{body_text}} {silent}");
            let loaded = loaded_with_prompt(Some(&template), true);
            assert!(prompt_messages(&loaded).is_empty(), "{silent}");
        }
        let loaded = loaded_with_prompt(Some("{{title}} {body_text} {_x1}"), true);
        assert_eq!(prompt_messages(&loaded).len(), 1);
    }

    #[test]
    fn many_unclosed_braces_stay_silent() {
        // Each `{` only scans its own identifier run; this input was quadratic
        // when every `{` searched the rest of the template for `}`.
        let template = format!("{{title}} {{body_text}}{}", "{".repeat(200_000));
        let loaded = loaded_with_prompt(Some(&template), true);
        assert!(prompt_messages(&loaded).is_empty());
    }

    #[test]
    fn prompt_checks_are_skipped_when_ai_is_disabled() {
        let loaded = loaded_with_prompt(Some("no placeholders"), false);
        assert!(prompt_messages(&loaded).is_empty());
    }

    #[test]
    fn config_warnings_lists_app_fields_before_category_prompts() {
        let mut loaded = loaded_with_prompt(Some("{title} {body_text} {summary}"), true);
        loaded.app.http.max_retries = 9;
        let warnings = config_warnings(&loaded);
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].field, "http.max_retries");
        assert_eq!(warnings[0].category, None);
        assert_eq!(warnings[1].code, "prompt_template");
        assert_eq!(warnings[1].category.as_deref(), Some("ai"));
    }
}
