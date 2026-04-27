use async_openai::error::OpenAIError;
use rss_ai_news_domain::error::ClassifiedError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AiError {
    #[error("http timeout after {seconds}s")]
    HttpTimeout { seconds: u64 },

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("http status {code}: {message}")]
    HttpStatus { code: u16, message: String },

    #[error("rate limited (HTTP 429): {message}")]
    RateLimited {
        message: String,
        retry_after_seconds: Option<u64>,
    },

    #[error("quota exceeded: {message}")]
    QuotaExceeded { message: String },

    #[error("invalid json response: {0}")]
    InvalidJson(String),

    #[error("schema invalid: missing required field `{field}`")]
    MissingField { field: String },

    #[error("schema invalid: field `{field}` value invalid: {reason}")]
    InvalidFieldValue { field: String, reason: String },

    #[error("response empty (no choices)")]
    EmptyResponse,

    #[error("config invalid: {0}")]
    InvalidConfig(String),
}

impl ClassifiedError for AiError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::HttpTimeout { .. } | Self::ConnectionFailed(_) | Self::RateLimited { .. } => true,
            Self::HttpStatus { code, .. } => *code >= 500,
            Self::QuotaExceeded { .. }
            | Self::InvalidJson(_)
            | Self::MissingField { .. }
            | Self::InvalidFieldValue { .. }
            | Self::EmptyResponse
            | Self::InvalidConfig(_) => false,
        }
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::HttpTimeout { .. } => "http_timeout",
            Self::ConnectionFailed(_) => "connection_failed",
            Self::HttpStatus { .. } => "http_status",
            Self::RateLimited { .. } => "rate_limited",
            Self::QuotaExceeded { .. } => "quota_exceeded",
            Self::InvalidJson(_) => "invalid_json",
            Self::MissingField { .. } => "missing_field",
            Self::InvalidFieldValue { .. } => "invalid_field_value",
            Self::EmptyResponse => "empty_response",
            Self::InvalidConfig(_) => "invalid_config",
        }
    }

    fn display_user(&self) -> String {
        match self {
            Self::HttpTimeout { seconds } => format!("AI request timed out after {seconds}s"),
            Self::ConnectionFailed(message) => format!("AI connection failed: {message}"),
            Self::HttpStatus { code, message } => {
                format!("AI provider returned HTTP {code}: {message}")
            }
            Self::RateLimited {
                message,
                retry_after_seconds,
            } => match retry_after_seconds {
                Some(seconds) => {
                    format!("AI provider rate limited request; retry after {seconds}s: {message}")
                }
                None => format!("AI provider rate limited request: {message}"),
            },
            Self::QuotaExceeded { message } => format!("AI quota exceeded: {message}"),
            Self::InvalidJson(message) => format!("AI response was not valid JSON: {message}"),
            Self::MissingField { field } => format!("AI response missing required field `{field}`"),
            Self::InvalidFieldValue { field, reason } => {
                format!("AI response field `{field}` is invalid: {reason}")
            }
            Self::EmptyResponse => "AI response contained no choices".to_string(),
            Self::InvalidConfig(message) => format!("AI config invalid: {message}"),
        }
    }

    fn display_debug(&self) -> String {
        format!("{self:?}")
    }
}

impl From<OpenAIError> for AiError {
    fn from(value: OpenAIError) -> Self {
        match value {
            OpenAIError::Reqwest(err) => {
                if err.is_timeout() {
                    return Self::HttpTimeout { seconds: 0 };
                }

                if let Some(status) = err.status() {
                    return classify_http_status(
                        status.as_u16(),
                        err.to_string(),
                        retry_after_seconds_from_reqwest_error(&err),
                    );
                }

                Self::ConnectionFailed(err.to_string())
            }
            OpenAIError::ApiError(err) => {
                if is_quota_error(err.r#type.as_deref(), err.code.as_deref(), &err.message) {
                    return Self::QuotaExceeded {
                        message: err.message,
                    };
                }

                if is_rate_limit_error(err.r#type.as_deref(), err.code.as_deref(), &err.message) {
                    return Self::RateLimited {
                        message: err.message,
                        retry_after_seconds: None,
                    };
                }

                Self::ConnectionFailed(err.to_string())
            }
            OpenAIError::JSONDeserialize(err) => Self::InvalidJson(err.to_string()),
            OpenAIError::InvalidArgument(message) => Self::InvalidConfig(message),
            OpenAIError::FileSaveError(message)
            | OpenAIError::FileReadError(message)
            | OpenAIError::StreamError(message) => Self::ConnectionFailed(message),
        }
    }
}

pub(crate) fn classify_http_status(
    code: u16,
    message: String,
    retry_after_seconds: Option<u64>,
) -> AiError {
    if code == 429 {
        if is_quota_message(&message) {
            return AiError::QuotaExceeded { message };
        }

        return AiError::RateLimited {
            message,
            retry_after_seconds,
        };
    }

    AiError::HttpStatus { code, message }
}

pub(crate) fn is_quota_error(error_type: Option<&str>, code: Option<&str>, message: &str) -> bool {
    error_type == Some("insufficient_quota")
        || code == Some("insufficient_quota")
        || is_quota_message(message)
}

pub(crate) fn is_rate_limit_error(
    error_type: Option<&str>,
    code: Option<&str>,
    message: &str,
) -> bool {
    error_type.is_some_and(|value| value.contains("rate_limit"))
        || code.is_some_and(|value| value.contains("rate_limit"))
        || message.to_ascii_lowercase().contains("rate limit")
}

fn is_quota_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("quota") || message.contains("insufficient_quota")
}

fn retry_after_seconds_from_reqwest_error(_err: &reqwest::Error) -> Option<u64> {
    None
}
