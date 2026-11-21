//! Extractor crate error classification.

use rss_ai_news_domain::error::ClassifiedError;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExtractorError {
    #[error("HTTP timeout")]
    HttpTimeout,
    #[error("HTTP status {code}")]
    HttpStatus { code: u16 },
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
            Self::HttpStatus { code: 429 } => "http_429",
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
