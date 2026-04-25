//! Shared error traits and classification.

/// All domain-level errors must implement this trait.
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
