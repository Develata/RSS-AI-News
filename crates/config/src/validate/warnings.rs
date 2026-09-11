use serde::Serialize;

use crate::{AppConfig, RetentionPolicy};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigWarning {
    pub code: &'static str,
    pub field: &'static str,
    pub message: &'static str,
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
        })
        .collect()
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
        assert_eq!(warnings.len(), 15);
        let mut fields: Vec<_> = warnings.iter().map(|warning| warning.field).collect();
        fields.sort_unstable();
        fields.dedup();
        assert_eq!(fields.len(), 15);
        assert!(
            !serde_json::to_string(&warnings)
                .unwrap()
                .contains("sensitive-fixture-value")
        );
    }
}
