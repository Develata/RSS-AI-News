use rss_ai_news_domain::error::ClassifiedError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PublishError {
    #[error("local IO failure: {0}")]
    LocalIoError(#[from] std::io::Error),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("github auth failed: {0}")]
    GitHubAuthFailed(String),
    #[error("github api error: status {status}")]
    GitHubApiError { status: u16, message: String },
    #[error("remote response body exceeds {limit} bytes")]
    ResponseTooLarge { limit: usize },
    #[error("remote publish deadline exceeded")]
    RemoteTimeout,
    #[error("github rate limit until {reset_at}")]
    GitHubRateLimit { reset_at: time::OffsetDateTime },
}

impl ClassifiedError for PublishError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::LocalIoError(error) => matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
            ),
            Self::InvalidPath(_) | Self::GitHubAuthFailed(_) | Self::ResponseTooLarge { .. } => {
                false
            }
            Self::GitHubApiError { status, .. } => *status == 409 || *status >= 500,
            Self::GitHubRateLimit { .. } | Self::RemoteTimeout => true,
        }
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::LocalIoError(_) => "local_io_error",
            Self::InvalidPath(_) => "invalid_path",
            Self::GitHubAuthFailed(_) => "github_auth_failed",
            Self::GitHubApiError { .. } => "github_api_error",
            Self::GitHubRateLimit { .. } => "github_rate_limit",
            Self::RemoteTimeout => "remote_timeout",
            Self::ResponseTooLarge { .. } => "response_too_large",
        }
    }

    fn display_user(&self) -> String {
        format!("{self}")
    }

    fn display_debug(&self) -> String {
        format!("{self:?}")
    }
}
