use rss_ai_news_domain::dto::publish::AiBindingError;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_storage::StorageError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReportError {
    #[error("snapshot empty: no candidates matched selection")]
    SnapshotEmpty,
    #[error("invalid candidate row: {0}")]
    InvalidCandidate(#[from] AiBindingError),
    #[error("invalid score: {0}")]
    InvalidScore(String),
    #[error("invalid tags json: {0}")]
    InvalidTagsJson(String),
    #[error("render failed: {0}")]
    RenderFailed(String),
    /// A repository read failed. Kept typed (not stringified) so callers can
    /// tell a transient DB failure (pool timeout, busy, dropped connection)
    /// from a business verdict and route it through the retryable release.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl ClassifiedError for ReportError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Storage(error) => error.is_retryable(),
            _ => false,
        }
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::SnapshotEmpty => "snapshot_empty",
            Self::InvalidCandidate(_) => "candidate_invalid",
            Self::InvalidScore(_) => "score_invalid",
            Self::InvalidTagsJson(_) => "tags_json_invalid",
            Self::RenderFailed(_) => "render_failed",
            Self::Storage(error) => error.error_kind(),
        }
    }

    fn display_user(&self) -> String {
        format!("{self}")
    }
}
