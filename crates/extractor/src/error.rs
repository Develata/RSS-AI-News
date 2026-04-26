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
            Self::HttpStatus { code } => *code >= 500,
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

    fn display_debug(&self) -> String {
        format!("{self:?}")
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
