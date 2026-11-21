//! Extractor crate error classification.

use rss_ai_news_domain::error::ClassifiedError;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExtractorError {
    #[error("HTTP timeout")]
    HttpTimeout,
    #[error("HTTP status {code}")]
    HttpStatus { code: u16 },
    /// HTTP 429 (R2-S10). `retry_after_seconds` is the response's
    /// `Retry-After` as a delay from the fetch (seconds form, or an HTTP-date
    /// minus now; a past date is 0), capped at [`MAX_RETRY_AFTER_SECONDS`];
    /// `None` when absent or invalid.
    #[error("HTTP status 429")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("connection failed")]
    ConnectionFailed,
    #[error("response too large: {bytes} bytes")]
    TooLarge { bytes: u64 },
    #[error("HTML parse failed: {reason}")]
    ParseFailed { reason: String },
    #[error("content too short: {chars} chars")]
    ContentTooShort { chars: u32 },
    #[error("invalid url: {url}")]
    InvalidUrl { url: String },
}

impl ClassifiedError for ExtractorError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::HttpTimeout => true,
            // 429 is rate limiting: retry after the cooldown instead of
            // falling back to the summary. 403 / 404 stay permanent (W18).
            Self::HttpStatus { code } => *code == 429 || *code >= 500,
            Self::RateLimited { .. } => true,
            Self::ConnectionFailed => true,
            Self::TooLarge { .. } => false,
            Self::ParseFailed { .. } => false,
            Self::ContentTooShort { .. } => false,
            Self::InvalidUrl { .. } => false,
        }
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::HttpTimeout => "http_timeout",
            Self::HttpStatus { code } if *code >= 500 => "http_5xx",
            Self::HttpStatus { code: 429 } | Self::RateLimited { .. } => "http_429",
            Self::HttpStatus { .. } => "http_4xx",
            Self::ConnectionFailed => "connection_failed",
            Self::TooLarge { .. } => "too_large",
            Self::ParseFailed { .. } => "html_parse",
            Self::ContentTooShort { .. } => "content_too_short",
            Self::InvalidUrl { .. } => "invalid_url",
        }
    }

    fn display_user(&self) -> String {
        format!("{self}")
    }
}

/// Longest server-requested wait honoured (one day): a larger `Retry-After`
/// is clamped so a misbehaving server cannot park an entry indefinitely.
pub const MAX_RETRY_AFTER_SECONDS: u64 = 24 * 60 * 60;

impl ExtractorError {
    /// The server-requested wait before retrying (R2-S10), if any.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::RateLimited {
                retry_after_seconds: Some(seconds),
            } => Some(std::time::Duration::from_secs(*seconds)),
            _ => None,
        }
    }
}

/// `Retry-After` (RFC 9110 §10.2.3) as seconds from `now`: a non-negative
/// integer, or an HTTP-date (a past date is 0). `None` when unparsable.
pub(crate) fn parse_retry_after(value: &str, now: std::time::SystemTime) -> Option<u64> {
    let value = value.trim();
    let seconds = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        // Too many digits to fit is still "a very long wait".
        value.parse::<u64>().unwrap_or(u64::MAX)
    } else {
        let at = httpdate::parse_http_date(value).ok()?;
        match at.duration_since(now) {
            // Round a partial second up: never retry before the date.
            Ok(delay) => delay.as_secs() + u64::from(delay.subsec_nanos() > 0),
            Err(_) => 0,
        }
    };
    Some(seconds.min(MAX_RETRY_AFTER_SECONDS))
}

impl From<reqwest::Error> for ExtractorError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            Self::HttpTimeout
        } else if err.is_connect() {
            Self::ConnectionFailed
        } else if let Some(status) = err.status() {
            Self::HttpStatus {
                code: status.as_u16(),
            }
        } else {
            Self::ConnectionFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification_retries_429_and_5xx_only() {
        let status = |code| ExtractorError::HttpStatus { code };
        assert!(status(429).is_retryable());
        assert_eq!(status(429).error_kind(), "http_429");
        assert!(status(500).is_retryable());
        // 403 / 404 stay permanent so the W18 summary fallback applies.
        for code in [400, 403, 404, 408, 410] {
            assert!(!status(code).is_retryable(), "{code}");
            assert_eq!(status(code).error_kind(), "http_4xx");
        }
    }
}
