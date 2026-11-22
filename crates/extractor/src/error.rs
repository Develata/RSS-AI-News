//! Extractor crate error classification.

use rss_ai_news_domain::error::ClassifiedError;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExtractorError {
    #[error("HTTP timeout")]
    HttpTimeout,
    #[error("HTTP status {code}")]
    HttpStatus { code: u16 },
    /// HTTP 429 (R2-S10, R3E-S10). `retry_at` is the response's
    /// `Retry-After` as an absolute instant fixed when the response was
    /// received (seconds form: receipt + delay; HTTP-date: that date, a past
    /// date stays in the past), capped at receipt +
    /// [`MAX_RETRY_AFTER_SECONDS`]; `None` when absent or invalid.
    #[error("HTTP status 429")]
    RateLimited {
        retry_at: Option<std::time::SystemTime>,
    },
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

/// Longest server-requested wait honoured (one day after the response was
/// received): a later `Retry-After` is clamped so a misbehaving server cannot
/// park an entry indefinitely. A legitimately longer wait (e.g. 48 h) is
/// therefore cut to one day; the early retry may meet another 429 and spends
/// one attempt of the retry budget (R3E-S10, plan/02 §8).
pub const MAX_RETRY_AFTER_SECONDS: u64 = 24 * 60 * 60;

impl ExtractorError {
    /// The server-requested earliest retry instant (R2-S10, R3E-S10), if any.
    pub fn retry_at(&self) -> Option<std::time::SystemTime> {
        match self {
            Self::RateLimited { retry_at } => *retry_at,
            _ => None,
        }
    }
}

/// `Retry-After` (RFC 9110 §10.2.3) as an absolute instant, anchored at the
/// response's receipt `received_at`: a non-negative integer of seconds, or an
/// HTTP-date. The result never exceeds `received_at +
/// MAX_RETRY_AFTER_SECONDS`. `None` when unparsable (or not representable).
pub(crate) fn parse_retry_at(
    value: &str,
    received_at: std::time::SystemTime,
) -> Option<std::time::SystemTime> {
    let cap = received_at.checked_add(std::time::Duration::from_secs(MAX_RETRY_AFTER_SECONDS))?;
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        // Too many digits to fit is still "a very long wait": the cap.
        let seconds = value
            .parse::<u64>()
            .unwrap_or(u64::MAX)
            .min(MAX_RETRY_AFTER_SECONDS);
        return received_at.checked_add(std::time::Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.min(cap))
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
