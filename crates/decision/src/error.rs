//! Closed error taxonomy (ADR 0010 §5). Every non-2xx status has a variant and
//! keeps its status. Messages are fixed text plus a provider `code` that passed
//! an allow-list; error bodies are never echoed, because escaped JSON can hide
//! credentials that plain-text redaction would miss. Any error means "no
//! judgement", never "not important".

use rss_ai_news_domain::error::ClassifiedError;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecisionError {
    #[error("decision provider rejected the API key (HTTP {status})")]
    Unauthorized { status: u16 },
    #[error("decision request rejected (HTTP {status}{})", code_suffix(code))]
    Rejected { status: u16, code: Option<String> },
    #[error("decision request body too large (HTTP 413)")]
    PayloadTooLarge,
    #[error("decision provider refused the request encoding (HTTP 415)")]
    UnsupportedEncoding,
    /// The local request deadline passed.
    #[error("decision request timed out after {seconds}s")]
    Timeout { seconds: u64 },
    /// The provider answered HTTP 408.
    #[error("decision provider timed out waiting for the request (HTTP 408)")]
    RequestTimeout,
    #[error("decision provider rate limited the request (HTTP 429)")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("decision provider unavailable (HTTP {status}{})", code_suffix(code))]
    Unavailable { status: u16, code: Option<String> },
    #[error("decision provider server error (HTTP {status})")]
    Server { status: u16 },
    #[error("decision provider returned HTTP {status}")]
    HttpStatus { status: u16 },
    /// The connection could not be established; nothing was sent.
    #[error("decision provider connection failed")]
    Connection,
    /// The connection broke after the request may have been sent (while
    /// sending or reading the response).
    #[error("decision provider connection broke during the exchange")]
    Transport,
    #[error("decision response exceeded {limit} bytes")]
    ResponseTooLarge { limit: usize },
    #[error("invalid decision response: {reason}")]
    InvalidResponse { reason: String },
    #[error("invalid decision request: {0}")]
    InvalidRequest(String),
    #[error("invalid decision configuration: {0}")]
    InvalidConfig(String),
}

fn code_suffix(code: &Option<String>) -> String {
    code.as_deref()
        .map(|code| format!(", {code}"))
        .unwrap_or_default()
}

impl ClassifiedError for DecisionError {
    /// A hint only: Phase A never retries. Laya's 503 may also come from an
    /// input that fails deterministically, so callers must cap attempts.
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. }
                | Self::RequestTimeout
                | Self::RateLimited { .. }
                | Self::Unavailable { .. }
                | Self::Server { .. }
                | Self::Connection
                | Self::Transport
        )
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::Unauthorized { .. } => "decision_unauthorized",
            Self::Rejected { .. } => "decision_rejected",
            Self::PayloadTooLarge => "decision_payload_too_large",
            Self::UnsupportedEncoding => "decision_unsupported_encoding",
            Self::Timeout { .. } => "decision_timeout",
            Self::RequestTimeout => "decision_request_timeout",
            Self::RateLimited { .. } => "decision_rate_limited",
            Self::Unavailable { .. } => "decision_unavailable",
            Self::Server { .. } => "decision_server_error",
            Self::HttpStatus { .. } => "decision_http_status",
            Self::Connection => "decision_connection_failed",
            Self::Transport => "decision_transport_failed",
            Self::ResponseTooLarge { .. } => "decision_response_too_large",
            Self::InvalidResponse { .. } => "decision_invalid_response",
            Self::InvalidRequest(_) => "decision_invalid_request",
            Self::InvalidConfig(_) => "decision_invalid_config",
        }
    }

    fn display_user(&self) -> String {
        self.to_string()
    }

    fn display_debug(&self) -> String {
        format!("{self:?}")
    }
}

/// Map a non-2xx status. `code` is already allow-listed by the caller.
pub(crate) fn classify_status(
    status: u16,
    code: Option<String>,
    retry_after_seconds: Option<u64>,
) -> DecisionError {
    match status {
        401 | 403 => DecisionError::Unauthorized { status },
        400 | 422 => DecisionError::Rejected { status, code },
        413 => DecisionError::PayloadTooLarge,
        415 => DecisionError::UnsupportedEncoding,
        408 => DecisionError::RequestTimeout,
        429 => DecisionError::RateLimited {
            retry_after_seconds,
        },
        503 | 529 => DecisionError::Unavailable { status, code },
        500..=599 => DecisionError::Server { status },
        _ => DecisionError::HttpStatus { status },
    }
}

/// Short machine codes only (`VALIDATION_ERROR`, `max_tokens_exceeded`, ...):
/// anything else, or anything containing the key, is dropped.
pub(crate) fn allow_code(code: &str, api_key: &str) -> Option<String> {
    let well_formed = (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    (well_formed && (api_key.is_empty() || !code.contains(api_key))).then(|| code.to_string())
}

/// Extract a provider error code from a JSON error body without echoing it:
/// laya-server uses `{"detail":{"code":…}}`; Jev's body shape is not
/// documented, so try `code` and a string or object `error`.
pub(crate) fn error_code(body: &[u8], api_key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let candidates = [
        value.pointer("/detail/code"),
        value.get("code"),
        value.pointer("/error/code"),
        value.get("error").filter(|error| error.is_string()),
    ];
    candidates
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .find_map(|code| allow_code(code, api_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_class_has_a_variant_and_keeps_its_status() {
        assert_eq!(
            classify_status(401, None, None),
            DecisionError::Unauthorized { status: 401 }
        );
        assert_eq!(
            classify_status(403, None, None),
            DecisionError::Unauthorized { status: 403 }
        );
        assert!(matches!(
            classify_status(422, Some("VALIDATION_ERROR".into()), None),
            DecisionError::Rejected {
                status: 422,
                code: Some(_)
            }
        ));
        assert_eq!(
            classify_status(413, None, None),
            DecisionError::PayloadTooLarge
        );
        assert_eq!(
            classify_status(415, None, None),
            DecisionError::UnsupportedEncoding
        );
        assert_eq!(
            classify_status(408, None, None),
            DecisionError::RequestTimeout
        );
        assert_eq!(
            classify_status(429, None, Some(3)),
            DecisionError::RateLimited {
                retry_after_seconds: Some(3)
            }
        );
        assert!(matches!(
            classify_status(529, None, None),
            DecisionError::Unavailable { status: 529, .. }
        ));
        assert_eq!(
            classify_status(502, None, None),
            DecisionError::Server { status: 502 }
        );
        assert_eq!(
            classify_status(418, None, None),
            DecisionError::HttpStatus { status: 418 }
        );
        assert_eq!(
            classify_status(302, None, None),
            DecisionError::HttpStatus { status: 302 }
        );
    }

    #[test]
    fn retryable_is_only_a_transient_hint() {
        for retryable in [
            classify_status(503, None, None),
            classify_status(500, None, None),
            classify_status(429, None, None),
            DecisionError::Connection,
        ] {
            assert!(retryable.is_retryable(), "{retryable:?}");
        }
        for permanent in [
            classify_status(401, None, None),
            classify_status(422, None, None),
            DecisionError::InvalidResponse { reason: "x".into() },
        ] {
            assert!(!permanent.is_retryable(), "{permanent:?}");
        }
    }

    #[test]
    fn error_codes_are_allow_listed_and_never_carry_the_key() {
        let laya = br#"{"detail":{"code":"MODEL_NOT_AVAILABLE","message":"x"}}"#;
        assert_eq!(
            error_code(laya, "sk-test-key-123"),
            Some("MODEL_NOT_AVAILABLE".into())
        );
        let jev = br#"{"error":"max_tokens_exceeded"}"#;
        assert_eq!(
            error_code(jev, "sk-test-key-123"),
            Some("max_tokens_exceeded".into())
        );
        let prose = br#"{"error":"the key sk-abc is invalid"}"#;
        assert_eq!(error_code(prose, "sk-abc"), None);
        let keyed = br#"{"code":"sk-abc"}"#;
        assert_eq!(error_code(keyed, "sk-abc"), None);
        assert_eq!(error_code(b"<html>oops</html>", "sk-test-key-123"), None);
    }
}
