//! Feed crate error classification.

use rss_ai_news_domain::error::ClassifiedError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    HttpTimeout,
    HttpStatus { code: u16 },
    ConnectionFailed { source: String },
    ParseFailed { reason: String },
    TooLarge { bytes: u64 },
    InvalidUrl { url: String },
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HttpTimeout => f.write_str("feed fetch timed out"),
            Self::HttpStatus { code } => write!(f, "feed HTTP status {code}"),
            Self::ConnectionFailed { source } => write!(f, "feed connection failed: {source}"),
            Self::ParseFailed { reason } => write!(f, "feed parse failed: {reason}"),
            Self::TooLarge { bytes } => write!(f, "feed payload too large: {bytes} bytes"),
            Self::InvalidUrl { url } => write!(f, "invalid feed URL: {url}"),
        }
    }
}

impl std::error::Error for FeedError {}

impl ClassifiedError for FeedError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::HttpTimeout | Self::ConnectionFailed { .. } => true,
            Self::HttpStatus { code } => (500..600).contains(code),
            Self::ParseFailed { .. } | Self::TooLarge { .. } | Self::InvalidUrl { .. } => false,
        }
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::HttpTimeout => "http_timeout",
            Self::HttpStatus { code } if (500..600).contains(code) => "http_5xx",
            Self::HttpStatus { .. } => "http_4xx",
            Self::ConnectionFailed { .. } => "connection_failed",
            Self::ParseFailed { .. } => "feed_parse",
            Self::TooLarge { .. } => "too_large",
            Self::InvalidUrl { .. } => "invalid_url",
        }
    }

    fn display_user(&self) -> String {
        match self {
            Self::HttpTimeout => "Feed 抓取超时".to_string(),
            Self::HttpStatus { code } => format!("Feed HTTP 状态异常：{code}"),
            Self::ConnectionFailed { .. } => "Feed 连接失败".to_string(),
            Self::ParseFailed { .. } => "Feed 解析失败".to_string(),
            Self::TooLarge { .. } => "Feed 响应过大".to_string(),
            Self::InvalidUrl { .. } => "Feed URL 无效".to_string(),
        }
    }

    fn display_debug(&self) -> String {
        match self {
            Self::HttpTimeout => "FeedError::HttpTimeout".to_string(),
            Self::HttpStatus { code } => format!("FeedError::HttpStatus {{ code: {code} }}"),
            Self::ConnectionFailed { source } => {
                format!("FeedError::ConnectionFailed {{ source: {source} }}")
            }
            Self::ParseFailed { reason } => {
                format!("FeedError::ParseFailed {{ reason: {reason} }}")
            }
            Self::TooLarge { bytes } => format!("FeedError::TooLarge {{ bytes: {bytes} }}"),
            Self::InvalidUrl { url } => format!("FeedError::InvalidUrl {{ url: {url} }}"),
        }
    }
}

impl From<reqwest::Error> for FeedError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            return Self::HttpTimeout;
        }

        if let Some(status) = error.status() {
            return Self::HttpStatus {
                code: status.as_u16(),
            };
        }

        if error.is_connect() || error.is_request() || error.is_builder() {
            return Self::ConnectionFailed {
                source: error.to_string(),
            };
        }

        Self::ConnectionFailed {
            source: error.to_string(),
        }
    }
}
