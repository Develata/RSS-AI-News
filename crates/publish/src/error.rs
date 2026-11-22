use rss_ai_news_domain::error::ClassifiedError;
use thiserror::Error;

/// Where a GitHub failure happened (R1-P-S1): the REST operation, the HTTP
/// status GitHub (or an intermediary) actually answered, and GitHub's request
/// id. Kept as fields rather than only text, so the runtime can persist them
/// in `publish_failed` events. Every field is optional: a transport failure
/// has no status, a client-side error no operation. The request id is only
/// ever a value validated by the target (GitHub's shape, never the token).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitHubRequestContext {
    pub operation: Option<&'static str>,
    pub http_status: Option<u16>,
    pub request_id: Option<String>,
}

impl GitHubRequestContext {
    /// `"{operation}: "` or nothing.
    fn prefix(&self) -> String {
        self.operation
            .map(|operation| format!("{operation}: "))
            .unwrap_or_default()
    }

    /// `" (http 201)"` when the answered status differs from `shown`, then
    /// `" (x-github-request-id: …)"` when known.
    fn suffix(&self, shown: Option<u16>) -> String {
        let mut suffix = String::new();
        if let Some(http_status) = self.http_status.filter(|status| Some(*status) != shown) {
            suffix.push_str(&format!(" (http {http_status})"));
        }
        if let Some(id) = &self.request_id {
            suffix.push_str(&format!(" (x-github-request-id: {id})"));
        }
        suffix
    }
}

/// `error_kind` of [`PublishError::RemoteWriteUnverified`] and
/// [`PublishError::EarlierWriteUnverified`]. A record released with it is
/// recovered by a read-only HEAD check on its next remote claim (R4-1).
pub const REMOTE_WRITE_UNVERIFIED_KIND: &str = "remote_write_unverified";

#[derive(Debug, Error)]
pub enum PublishError {
    #[error("local IO failure: {0}")]
    LocalIoError(#[from] std::io::Error),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// 401, or a 403 without a rate-limit signal; `message` is the sanitized
    /// reason (R1-P-B6), the actual status is in `context.http_status`.
    #[error(
        "github auth failed: {}{}{message}{}",
        .context.prefix(),
        .context.http_status.map(|status| format!("status {status}: ")).unwrap_or_default(),
        .context.suffix(.context.http_status)
    )]
    GitHubAuthFailed {
        message: String,
        context: GitHubRequestContext,
    },
    /// `status` is the classification status (e.g. 502 for a 2xx missing a
    /// required field, 503 for a transport failure); `message` is sanitized
    /// (fixed-vocabulary GitHub reason or a bounded client message).
    #[error(
        "github api error: status {status}: {}{message}{}",
        .context.prefix(),
        .context.suffix(Some(*status))
    )]
    GitHubApiError {
        status: u16,
        message: String,
        context: GitHubRequestContext,
    },
    #[error(
        "remote response body exceeds {limit} bytes{}{}",
        .context.operation.map(|operation| format!(" ({operation})")).unwrap_or_default(),
        .context.suffix(None)
    )]
    ResponseTooLarge {
        limit: usize,
        context: GitHubRequestContext,
    },
    #[error("remote publish deadline exceeded")]
    RemoteTimeout,
    #[error(
        "github rate limit until {reset_at}{}{}",
        .context.operation.map(|operation| format!(" ({operation})")).unwrap_or_default(),
        .context.suffix(None)
    )]
    GitHubRateLimit {
        reset_at: time::OffsetDateTime,
        context: GitHubRequestContext,
    },
    /// R3F-B2: a write whose outcome is unknown (`write`: a transport failure,
    /// unreadable response or 5xx after the PUT / PATCH was sent, or the
    /// remote deadline cutting the call off) whose read-only HEAD
    /// verification did not complete (`verification`: the check failed, was
    /// rate limited or ran out of time). The write may have landed, so the
    /// stage must neither fail the record nor release its snapshot's articles:
    /// the runtime refunds the attempt and keeps the state (the next attempt's
    /// pre-write HEAD check is idempotent), waiting out a rate limit the
    /// verification returned ([`Self::rate_limit_reset`]).
    #[error("github write outcome unknown ({write}); HEAD verification incomplete: {verification}")]
    RemoteWriteUnverified {
        write: Box<PublishError>,
        verification: Box<PublishError>,
    },
    /// R4-1: the record's previous attempt ended [`Self::RemoteWriteUnverified`]
    /// (its write may have landed), and this attempt could not settle that
    /// before writing: the read-only fixed-HEAD content check failed, was
    /// rate limited or ran out of time, or a step before it (preparation)
    /// failed retryably (`verification`). No write was sent in this attempt.
    /// Released like [`Self::RemoteWriteUnverified`] (same `error_kind`, so
    /// the next claim recovers again); the request context and rate-limit
    /// deadline are the failed check's.
    #[error("github write of an earlier attempt still unverified: {verification}")]
    EarlierWriteUnverified { verification: Box<PublishError> },
    /// The record was initialised for a different remote destination than the
    /// one configured now. Never retried: the operator decides (restore the
    /// config, or `--force` a new render version for the new target).
    #[error(
        "remote target mismatch: record was initialised for {}, configured target is {current}",
        stored.as_deref().unwrap_or("<none>")
    )]
    RemoteTargetMismatch {
        stored: Option<String>,
        current: String,
    },
}

impl PublishError {
    /// GitHub request context of a GitHub failure, `None` for every other
    /// variant (and for a GitHub failure created without one).
    pub fn github_context(&self) -> Option<&GitHubRequestContext> {
        match self {
            Self::GitHubAuthFailed { context, .. }
            | Self::GitHubApiError { context, .. }
            | Self::ResponseTooLarge { context, .. }
            | Self::GitHubRateLimit { context, .. } => Some(context),
            // Where the unknown write was sent.
            Self::RemoteWriteUnverified { write, .. } => write.github_context(),
            // No write in this attempt: where the check failed.
            Self::EarlierWriteUnverified { verification } => verification.github_context(),
            _ => None,
        }
    }

    /// The instant GitHub asked to wait until: a rate limit's `reset_at`, or
    /// the rate limit an unverified write's HEAD verification ran into.
    pub fn rate_limit_reset(&self) -> Option<time::OffsetDateTime> {
        match self {
            Self::GitHubRateLimit { reset_at, .. } => Some(*reset_at),
            Self::RemoteWriteUnverified { verification, .. }
            | Self::EarlierWriteUnverified { verification } => verification.rate_limit_reset(),
            _ => None,
        }
    }

    /// Mutable access for attaching the request context after classification.
    pub(crate) fn github_context_mut(&mut self) -> Option<&mut GitHubRequestContext> {
        match self {
            Self::GitHubAuthFailed { context, .. }
            | Self::GitHubApiError { context, .. }
            | Self::ResponseTooLarge { context, .. }
            | Self::GitHubRateLimit { context, .. } => Some(context),
            _ => None,
        }
    }

    /// `GitHubApiError` without request context (client-side or test errors).
    pub fn api(status: u16, message: impl Into<String>) -> Self {
        Self::GitHubApiError {
            status,
            message: message.into(),
            context: GitHubRequestContext::default(),
        }
    }

    /// `GitHubAuthFailed` without request context.
    pub fn auth(message: impl Into<String>) -> Self {
        Self::GitHubAuthFailed {
            message: message.into(),
            context: GitHubRequestContext::default(),
        }
    }

    /// `GitHubRateLimit` without request context.
    pub fn rate_limit(reset_at: time::OffsetDateTime) -> Self {
        Self::GitHubRateLimit {
            reset_at,
            context: GitHubRequestContext::default(),
        }
    }
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
            Self::InvalidPath(_)
            | Self::GitHubAuthFailed { .. }
            | Self::ResponseTooLarge { .. }
            | Self::RemoteTargetMismatch { .. } => false,
            Self::GitHubApiError { status, .. } => *status == 409 || *status >= 500,
            Self::GitHubRateLimit { .. }
            | Self::RemoteTimeout
            | Self::RemoteWriteUnverified { .. }
            | Self::EarlierWriteUnverified { .. } => true,
        }
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::LocalIoError(_) => "local_io_error",
            Self::InvalidPath(_) => "invalid_path",
            Self::GitHubAuthFailed { .. } => "github_auth_failed",
            Self::GitHubApiError { .. } => "github_api_error",
            Self::GitHubRateLimit { .. } => "github_rate_limit",
            Self::RemoteTimeout => "remote_timeout",
            Self::RemoteWriteUnverified { .. } | Self::EarlierWriteUnverified { .. } => {
                REMOTE_WRITE_UNVERIFIED_KIND
            }
            Self::ResponseTooLarge { .. } => "response_too_large",
            Self::RemoteTargetMismatch { .. } => "remote_target_mismatch",
        }
    }

    fn display_user(&self) -> String {
        format!("{self}")
    }
}
