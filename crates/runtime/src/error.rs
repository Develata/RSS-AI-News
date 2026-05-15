use rss_ai_news_ai::AiError;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_extractor::ExtractorError;
use rss_ai_news_feed::FeedError;
use rss_ai_news_publish::PublishError;
use rss_ai_news_report::ReportError;
use rss_ai_news_storage::StorageError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("feed: {0}")]
    Feed(#[from] FeedError),
    #[error("extractor: {0}")]
    Extractor(#[from] ExtractorError),
    #[error("ai: {0}")]
    Ai(#[from] AiError),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("report: {0}")]
    Report(#[from] ReportError),
    #[error("publish: {0}")]
    Publish(#[from] PublishError),
    #[error("link normalize: {0}")]
    LinkNormalize(String),
    #[error("config: {0}")]
    Config(String),
    /// 持有的 lease 已失效（被 reclaim 或被外部状态变更覆盖），
    /// 当前 worker 不再是该行的 owner。docs/design/error-and-observability.md
    /// §2.1 line 33 规定的层二错误；F15-fix1 起 reindex flow 在 lease guard
    /// 失败时把它向上抛而不是 silent warn——避免"实际未完成、CLI 报告成功"
    /// 的假阳性。
    #[error("lease conflict on {table}#{id}: expected owner '{expected_owner}'")]
    LeaseConflict {
        table: &'static str,
        id: i64,
        expected_owner: String,
    },
    #[error("cancelled")]
    Cancelled,
}

impl ClassifiedError for RuntimeError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Feed(error) => error.is_retryable(),
            Self::Extractor(error) => error.is_retryable(),
            Self::Ai(error) => error.is_retryable(),
            Self::Storage(error) => error.is_retryable(),
            Self::Report(error) => error.is_retryable(),
            Self::Publish(error) => error.is_retryable(),
            Self::LinkNormalize(_)
            | Self::Config(_)
            | Self::LeaseConflict { .. }
            | Self::Cancelled => false,
        }
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::Feed(error) => error.error_kind(),
            Self::Extractor(error) => error.error_kind(),
            Self::Ai(error) => error.error_kind(),
            Self::Storage(error) => error.error_kind(),
            Self::Report(error) => error.error_kind(),
            Self::Publish(error) => error.error_kind(),
            Self::LinkNormalize(_) => "link_normalize",
            Self::Config(_) => "runtime_config",
            Self::LeaseConflict { .. } => "lease_conflict",
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
