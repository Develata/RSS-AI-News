use std::collections::HashSet;

use url::Url;

use crate::{
    AppConfig, CategoryConfig, Diagnostic, DiagnosticReport, EnvConfig, LoadedConfig,
    validate::CommandFlags,
};

use super::SUPPORTED_SCHEMA_VERSION;

pub(super) fn collect_general_checks(
    report: &mut DiagnosticReport,
    app: &AppConfig,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) {
    if app.schema_version != SUPPORTED_SCHEMA_VERSION {
        report.push(Diagnostic::new(
            "app.toml",
            "schema_version",
            format!(
                "expected schema_version {SUPPORTED_SCHEMA_VERSION:?}, got {:?}",
                app.schema_version
            ),
        ));
    }

    collect_category_checks(report, categories, env);
    collect_app_value_checks(report, app);
}

pub(super) fn collect_env_checks(
    report: &mut DiagnosticReport,
    app: &AppConfig,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) {
    if app.ai.enabled {
        if is_blank(
            env.openai_api_key
                .as_ref()
                .map(rss_ai_news_domain::SecretString::expose_secret),
        ) {
            report.push(Diagnostic::new(
                ".env",
                "OPENAI_API_KEY",
                "required when app.ai.enabled=true",
            ));
        }
        match env
            .openai_base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            Some(value) if Url::parse(value).is_ok() => {}
            Some(value) => report.push(Diagnostic::new(
                ".env",
                "OPENAI_BASE_URL",
                format!("invalid URL {value:?}"),
            )),
            None => report.push(Diagnostic::new(
                ".env",
                "OPENAI_BASE_URL",
                "required when app.ai.enabled=true",
            )),
        }
    }

    if categories
        .iter()
        .flat_map(|category| &category.sources)
        .any(|source| source.feed_url.contains("{RSSHUB}"))
        && is_blank(env.rsshub_base_url.as_deref())
    {
        report.push(Diagnostic::new(
            ".env",
            "RSSHUB_BASE_URL",
            "required because at least one source uses {RSSHUB} placeholder",
        ));
    }
}

pub(super) fn collect_publish_checks(
    report: &mut DiagnosticReport,
    config: &LoadedConfig,
    flags: &CommandFlags,
) {
    if flags.local_only || config.app.publish.github_owner.trim().is_empty() {
        return;
    }
    if is_blank(
        config
            .env
            .github_token
            .as_ref()
            .map(rss_ai_news_domain::SecretString::expose_secret),
    ) {
        report.push(Diagnostic::new(
            ".env",
            "GITHUB_TOKEN",
            "required for remote publish",
        ));
    }
    if config.app.publish.github_repo.trim().is_empty() {
        report.push(Diagnostic::new(
            "app.toml",
            "publish.github_repo",
            "required for remote publish when publish.github_owner is non-empty",
        ));
    }
}

fn collect_category_checks(
    report: &mut DiagnosticReport,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) {
    let mut category_keys = HashSet::new();
    for category in categories {
        let source_file = category_source_file(category);
        if category.schema_version != SUPPORTED_SCHEMA_VERSION {
            report.push(Diagnostic::new(
                source_file.clone(),
                "schema_version",
                format!(
                    "expected schema_version {SUPPORTED_SCHEMA_VERSION:?}, got {:?}",
                    category.schema_version
                ),
            ));
        }
        if !category_keys.insert(category.category.key.clone()) {
            report.push(Diagnostic::new(
                source_file.clone(),
                "category.key",
                format!("duplicate category key {:?}", category.category.key),
            ));
        }

        let mut source_keys = HashSet::new();
        for (index, source) in category.sources.iter().enumerate() {
            if !source_keys.insert(source.key.clone()) {
                report.push(Diagnostic::new(
                    source_file.clone(),
                    format!("sources[{index}].key"),
                    format!("duplicate source key {:?}", source.key),
                ));
            }
            validate_feed_url(report, &source_file, index, &source.feed_url, env);
        }
    }
}

fn collect_app_value_checks(report: &mut DiagnosticReport, app: &AppConfig) {
    if !is_supported_timezone(&app.publish.target_timezone) {
        report.push(Diagnostic::new(
            "app.toml",
            "publish.target_timezone",
            format!("invalid IANA timezone {:?}", app.publish.target_timezone),
        ));
    }

    let u64_checks = [
        ("http.timeout_seconds", app.http.timeout_seconds),
        ("http.retry_backoff_base_ms", app.http.retry_backoff_base_ms),
        (
            "lease.fetch_duration_seconds",
            app.lease.fetch_duration_seconds,
        ),
        ("lease.ai_duration_seconds", app.lease.ai_duration_seconds),
        (
            "lease.publish_duration_seconds",
            app.lease.publish_duration_seconds,
        ),
        (
            "lease.reclaim_interval_seconds",
            app.lease.reclaim_interval_seconds,
        ),
        ("extractor.max_body_bytes", app.extractor.max_body_bytes),
        ("ai.request_timeout_seconds", app.ai.request_timeout_seconds),
    ];
    for (field_path, value) in u64_checks {
        check_positive_u64(report, "app.toml", field_path, value);
    }

    let u32_checks = [
        ("http.concurrent_feeds", app.http.concurrent_feeds),
        ("http.concurrent_fetches", app.http.concurrent_fetches),
        (
            "retry.feed_entry_max_attempts",
            app.retry.feed_entry_max_attempts,
        ),
        ("retry.ai_max_attempts", app.retry.ai_max_attempts),
        ("retry.publish_max_attempts", app.retry.publish_max_attempts),
        ("extractor.min_body_chars", app.extractor.min_body_chars),
        ("ai.max_tokens", app.ai.max_tokens),
        (
            "ai.rate_limit.requests_per_minute",
            app.ai.rate_limit.requests_per_minute,
        ),
    ];
    for (field_path, value) in u32_checks {
        check_positive_u32(report, "app.toml", field_path, value);
    }
}

fn validate_feed_url(
    report: &mut DiagnosticReport,
    source_file: &str,
    index: usize,
    feed_url: &str,
    env: &EnvConfig,
) {
    if feed_url.trim().is_empty() {
        report.push(Diagnostic::new(
            source_file,
            format!("sources[{index}].feed_url"),
            "feed_url must not be empty",
        ));
        return;
    }

    let candidate = if feed_url.contains("{RSSHUB}") {
        match env
            .rsshub_base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            Some(base_url) => feed_url.replace("{RSSHUB}", base_url.trim_end_matches('/')),
            None => return,
        }
    } else {
        feed_url.to_string()
    };

    if Url::parse(&candidate).is_err() {
        report.push(Diagnostic::new(
            source_file,
            format!("sources[{index}].feed_url"),
            format!("invalid URL {feed_url:?}"),
        ));
    }
}

fn check_positive_u32(
    report: &mut DiagnosticReport,
    source_file: &str,
    field_path: &str,
    value: u32,
) {
    if value == 0 {
        report.push(Diagnostic::new(source_file, field_path, "must be > 0"));
    }
}

fn check_positive_u64(
    report: &mut DiagnosticReport,
    source_file: &str,
    field_path: &str,
    value: u64,
) {
    if value == 0 {
        report.push(Diagnostic::new(source_file, field_path, "must be > 0"));
    }
}

fn is_blank(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.trim().is_empty())
}

fn category_source_file(category: &CategoryConfig) -> String {
    format!("categories/{}.toml", category.category.key)
}

// Strict IANA parsing is intentionally deferred until the project adds a timezone
// database dependency. This whitelist covers shipped fixtures and common targets,
// then rejects blank or malformed strings with whitespace.
fn is_supported_timezone(timezone: &str) -> bool {
    matches!(
        timezone,
        "UTC"
            | "Asia/Shanghai"
            | "Asia/Tokyo"
            | "Asia/Singapore"
            | "Europe/London"
            | "Europe/Berlin"
            | "America/New_York"
            | "America/Los_Angeles"
    ) || (timezone.contains('/')
        && !timezone.trim().is_empty()
        && !timezone.chars().any(char::is_whitespace))
}
