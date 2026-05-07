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
///   `dto::publish::PublishCandidateError`). These are wrapped by upstream
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
