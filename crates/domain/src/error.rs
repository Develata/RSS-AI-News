//! Shared error traits and classification.

/// Trait for errors that flow through the cross-crate retry / failure-state /
/// observability dispatch.
///
/// **In scope** — required to impl `ClassifiedError`:
/// - The capability-layer error enums enumerated in
///   `docs/design/error-and-observability.md` §2.3: `FeedError`,
///   `ExtractorError`, `AiError`, `StorageError`, `PublishError`.
/// - Domain-layer errors that surface during a capability operation and are
///   classified by `runtime` for retry / failure-state transition. Today this
///   is `link_normalizer::LinkNormalizeError` (raised inside ingest).
///
/// **Out of scope** — must NOT be required to impl this trait:
/// - Pure construction-validation errors that signal API misuse rather than
///   external failure (e.g. `score::ScoreOutOfRange`,
///   `dto::publish::AiBindingError`). These are wrapped by upstream
///   capability errors before classification (see
///   `crates/report/src/error.rs::ReportError::InvalidCandidate` for a
///   concrete example) and never themselves drive retry decisions.
pub trait ClassifiedError {
    /// Whether this error is safe to retry.
    fn is_retryable(&self) -> bool;

    /// Machine-readable error kind for storage in `last_error_kind`.
    fn error_kind(&self) -> &str;

    /// User-facing error message (CLI output).
    fn display_user(&self) -> String;

    /// Debug-level error message (logs).
    fn display_debug(&self) -> String;
}

/// Bound a diagnostic by UTF-8 bytes, including its truncation marker.
/// Call after secret redaction and classification; this is not a raw-artifact limit.
///
/// # Panics
/// Panics if `max_bytes` is smaller than 64 (the marker budget).
pub fn truncate_diagnostic(mut message: String, max_bytes: usize) -> String {
    assert!(max_bytes >= 64);
    if message.len() <= max_bytes {
        return message;
    }
    let suffix = format!("… [truncated, original_bytes={}]", message.len());
    let mut end = max_bytes - suffix.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str(&suffix);
    // Do not retain a multi-MiB backing allocation in a small error value.
    message.shrink_to_fit();
    message
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn diagnostic_limit_includes_marker_and_preserves_utf8() {
        for text in ["a", "中", "🦀"] {
            let original = text.repeat(10000);
            let bounded = truncate_diagnostic(original.clone(), 8192);
            assert!(bounded.len() <= 8192);
            assert!(bounded.ends_with(&format!("[truncated, original_bytes={}]", original.len())));
            assert_eq!(truncate_diagnostic(bounded.clone(), 8192), bounded);
        }
        for len in [0, 8191, 8192] {
            let text = "a".repeat(len);
            assert_eq!(truncate_diagnostic(text.clone(), 8192), text);
        }
    }
}
