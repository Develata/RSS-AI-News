use rss_ai_news_config::ConfigError;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_runtime::RuntimeError;
use rss_ai_news_storage::StorageError;
use thiserror::Error;

use crate::exit_code::ExitCode;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Runtime(#[from] RuntimeError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Storage(#[from] StorageError),
    #[error("{feature}: 该命令在 W9b/W9c 接入 (not implemented yet)")]
    NotImplementedYet { feature: String },
}

impl CliError {
    pub fn error_kind(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Runtime(_) => "runtime",
            Self::Io(_) => "io",
            Self::Storage(_) => "storage",
            Self::NotImplementedYet { .. } => "not_implemented_yet",
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) => ExitCode::ConfigError,
            Self::Runtime(_) | Self::Io(_) | Self::Storage(_) | Self::NotImplementedYet { .. } => {
                ExitCode::RuntimeError
            }
        }
    }

    pub fn display_user(&self) -> String {
        match self {
            Self::Config(error) => error.to_string(),
            Self::Runtime(error) => error.display_user(),
            Self::Io(error) => format!("I/O error: {error}"),
            Self::Storage(error) => error.display_user(),
            Self::NotImplementedYet { feature } => {
                format!("{feature}: 该命令在 W9b/W9c 接入 (not implemented yet)")
            }
        }
    }

    pub fn command_name(&self) -> &str {
        match self {
            Self::NotImplementedYet { feature } => feature.as_str(),
            _ => "unknown",
        }
    }
}

impl From<rss_ai_news_feed::FeedError> for CliError {
    fn from(value: rss_ai_news_feed::FeedError) -> Self {
        Self::Runtime(RuntimeError::Feed(value))
    }
}

impl From<rss_ai_news_extractor::ExtractorError> for CliError {
    fn from(value: rss_ai_news_extractor::ExtractorError) -> Self {
        Self::Runtime(RuntimeError::Extractor(value))
    }
}

impl From<rss_ai_news_ai::AiError> for CliError {
    fn from(value: rss_ai_news_ai::AiError) -> Self {
        Self::Runtime(RuntimeError::Ai(value))
    }
}

impl From<rss_ai_news_publish::PublishError> for CliError {
    fn from(value: rss_ai_news_publish::PublishError) -> Self {
        Self::Runtime(RuntimeError::Publish(value))
    }
}
