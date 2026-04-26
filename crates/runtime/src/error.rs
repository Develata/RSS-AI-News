use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_feed::FeedError;
use rss_ai_news_storage::StorageError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("feed: {0}")]
    Feed(#[from] FeedError),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("link normalize: {0}")]
    LinkNormalize(String),
    #[error("config: {0}")]
    Config(String),
    #[error("cancelled")]
    Cancelled,
}

impl ClassifiedError for RuntimeError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Feed(error) => error.is_retryable(),
            Self::Storage(error) => error.is_retryable(),
            Self::LinkNormalize(_) | Self::Config(_) | Self::Cancelled => false,
        }
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::Feed(error) => error.error_kind(),
            Self::Storage(error) => error.error_kind(),
            Self::LinkNormalize(_) => "link_normalize",
            Self::Config(_) => "runtime_config",
            Self::Cancelled => "cancelled",
        }
    }

    fn display_user(&self) -> String {
        format!("{self}")
    }

    fn display_debug(&self) -> String {
        format!("{self:?}")
    }
}
