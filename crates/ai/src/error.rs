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

    #[error("model unavailable: {message}")]
    ModelUnavailable { message: String },

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

impl AiError {
    /// 该错误是否值得换模型重试（W14-A）。除"换 model 名 100% 无救"的两类外均 true：
    /// `InvalidConfig`（凭证/配置错，全局共用凭证下换模型必同样失败）与
    /// `ConnectionFailed`（连不上 endpoint，同 base_url 换 model 名照样连不上）。
    /// 见 docs/plan/14-ai-fallback.md §3。
    pub fn should_fallback(&self) -> bool {
        !matches!(self, Self::InvalidConfig(_) | Self::ConnectionFailed(_))
    }
}

impl ClassifiedError for AiError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::HttpTimeout { .. } | Self::ConnectionFailed(_) | Self::RateLimited { .. } => true,
            Self::HttpStatus { code, .. } => *code >= 500,
            Self::QuotaExceeded { .. }
            | Self::ModelUnavailable { .. }
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
            Self::ModelUnavailable { .. } => "model_unavailable",
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
            Self::ModelUnavailable { message } => format!("AI model unavailable: {message}"),
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

                if is_model_unavailable_error(
                    err.r#type.as_deref(),
                    err.code.as_deref(),
                    &err.message,
                ) {
                    return Self::ModelUnavailable {
                        message: err.message,
                    };
                }

                if is_rate_limit_error(err.r#type.as_deref(), err.code.as_deref(), &err.message) {
                    return Self::RateLimited {
                        message: err.message,
                        retry_after_seconds: None,
                    };
                }

                // 非 quota/rate/model 的 provider API 错误：async-openai 的 ApiError 不带
                // HTTP 状态码，用 code 0 标记"provider 返回的错误但无状态码"，避免误判为
                // ConnectionFailed（后者不触发 fallback，见 should_fallback）。
                Self::HttpStatus {
                    code: 0,
                    message: err.message,
                }
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

    if is_model_unavailable_message(&message) {
        return AiError::ModelUnavailable { message };
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

pub(crate) fn is_model_unavailable_error(
    error_type: Option<&str>,
    code: Option<&str>,
    message: &str,
) -> bool {
    matches!(
        error_type,
        Some("model_not_found") | Some("model_not_available")
    ) || matches!(code, Some("model_not_found") | Some("model_not_available"))
        || is_model_unavailable_message(message)
}

fn is_model_unavailable_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("model_not_found")
        || message.contains("model not found")
        || message.contains("model_not_available")
        || message.contains("does not exist")
        || message.contains("no such model")
}

fn is_quota_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("quota") || message.contains("insufficient_quota")
}

fn retry_after_seconds_from_reqwest_error(_err: &reqwest::Error) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::error::ApiError;
    use rss_ai_news_domain::error::ClassifiedError;

    #[test]
    fn should_fallback_excludes_only_config_and_connection() {
        // W14-A §3：仅 InvalidConfig / ConnectionFailed 不回退，其余全部回退。
        assert!(!AiError::InvalidConfig("x".into()).should_fallback());
        assert!(!AiError::ConnectionFailed("x".into()).should_fallback());

        for err in [
            AiError::QuotaExceeded {
                message: "x".into(),
            },
            AiError::RateLimited {
                message: "x".into(),
                retry_after_seconds: None,
            },
            AiError::HttpStatus {
                code: 500,
                message: "x".into(),
            },
            AiError::HttpStatus {
                code: 400,
                message: "x".into(),
            },
            AiError::ModelUnavailable {
                message: "x".into(),
            },
            AiError::HttpTimeout { seconds: 1 },
            AiError::InvalidJson("x".into()),
            AiError::MissingField { field: "x".into() },
            AiError::EmptyResponse,
        ] {
            assert!(err.should_fallback(), "expected fallback for {err:?}");
        }
    }

    #[test]
    fn model_unavailable_detected_by_type_code_and_message() {
        assert!(is_model_unavailable_error(
            Some("model_not_found"),
            None,
            "irrelevant"
        ));
        assert!(is_model_unavailable_error(
            None,
            Some("model_not_available"),
            "irrelevant"
        ));
        assert!(is_model_unavailable_error(
            None,
            None,
            "The model `gpt-x` does not exist"
        ));
        assert!(is_model_unavailable_error(None, None, "Model Not Found"));
        assert!(!is_model_unavailable_error(
            Some("invalid_request_error"),
            None,
            "bad param"
        ));
    }

    #[test]
    fn classify_http_status_maps_model_text_to_model_unavailable() {
        let err = classify_http_status(404, "model not found: gpt-x".to_string(), None);
        assert!(matches!(err, AiError::ModelUnavailable { .. }));
        assert_eq!(err.error_kind(), "model_unavailable");
        assert!(!err.is_retryable());
        assert!(err.should_fallback());
    }

    #[test]
    fn classify_http_status_429_and_5xx_unchanged() {
        assert!(matches!(
            classify_http_status(429, "rate limit reached".to_string(), Some(3)),
            AiError::RateLimited { .. }
        ));
        assert!(matches!(
            classify_http_status(503, "upstream".to_string(), None),
            AiError::HttpStatus { code: 503, .. }
        ));
    }

    #[test]
    fn from_openai_api_error_detects_model_and_routes_remainder_to_http_status() {
        let model_err = OpenAIError::ApiError(ApiError {
            message: "The model does not exist".to_string(),
            r#type: None,
            param: None,
            code: Some("model_not_found".to_string()),
        });
        assert!(matches!(
            AiError::from(model_err),
            AiError::ModelUnavailable { .. }
        ));

        // 非 quota/rate/model 的 provider 错误 → HttpStatus{0}（可 fallback），
        // 不再误判为 ConnectionFailed（后者不 fallback）。
        let other = OpenAIError::ApiError(ApiError {
            message: "bad request".to_string(),
            r#type: Some("invalid_request_error".to_string()),
            param: None,
            code: None,
        });
        let mapped = AiError::from(other);
        assert!(matches!(mapped, AiError::HttpStatus { code: 0, .. }));
        assert!(mapped.should_fallback());
    }
}
