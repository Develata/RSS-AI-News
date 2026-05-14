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
    #[error("doctor detected failing checks")]
    DoctorFailed,
    #[error("dry-run is not implemented for ingest yet")]
    DryRunNotImplemented,
    #[error("ingest --source is not implemented yet")]
    IngestSourceFilterNotImplemented,
    /// cli-semantics §4.8 line 290 + §6（参数错误 → exit 2）。`--abort
    /// <job_id>` 需要可解析的整数；非法输入归为 UserError。
    #[error("reindex --abort expects a positive integer job id, got: {raw}")]
    ReindexAbortInvalidJobId { raw: String },
    /// cli-semantics §4.8 line 287: `--target` 在非 `--abort` 模式下必填。
    /// clap 通过 `required_unless_present` 保证；这是兜底，正常分支不会触发。
    #[error("reindex requires --target unless --abort is given")]
    ReindexTargetRequired,
    #[error("replay artifact not found: {kind}/{key}")]
    ReplayArtifactNotFound { kind: String, key: String },
    #[error("publish record not found: {idempotency_key}")]
    PublishRecordNotFound { idempotency_key: String },
    #[error("publish record is in conflicting state: {state}")]
    PublishConflict { state: String },
}

impl CliError {
    pub fn error_kind(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Runtime(_) => "runtime",
            Self::Io(_) => "io",
            Self::Storage(_) => "storage",
            Self::DoctorFailed => "doctor_failed",
            Self::DryRunNotImplemented => "dry_run_not_implemented",
            Self::IngestSourceFilterNotImplemented => "ingest_source_not_implemented",
            Self::ReindexAbortInvalidJobId { .. } => "reindex_abort_invalid_job_id",
            Self::ReindexTargetRequired => "reindex_target_required",
            Self::ReplayArtifactNotFound { .. } => "replay_artifact_not_found",
            Self::PublishRecordNotFound { .. } => "publish_record_not_found",
            Self::PublishConflict { .. } => "publish_conflict",
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) => ExitCode::ConfigError,
            // §4.8 line 327: reindex 参数错误 → exit 2 (UserError)。
            // `ReindexTargetRequired` 与 `ReindexAbortInvalidJobId` 都是 clap
            // 没接住的输入校验失败，归类参数错误。
            Self::ReindexTargetRequired | Self::ReindexAbortInvalidJobId { .. } => {
                ExitCode::UserError
            }
            Self::Runtime(_)
            | Self::Io(_)
            | Self::Storage(_)
            | Self::DoctorFailed
            | Self::DryRunNotImplemented
            | Self::IngestSourceFilterNotImplemented
            | Self::ReplayArtifactNotFound { .. }
            | Self::PublishRecordNotFound { .. }
            | Self::PublishConflict { .. } => ExitCode::RuntimeError,
        }
    }

    pub fn display_user(&self) -> String {
        match self {
            Self::Config(error) => error.to_string(),
            Self::Runtime(error) => error.display_user(),
            Self::Io(error) => format!("I/O error: {error}"),
            Self::Storage(error) => error.display_user(),
            Self::DoctorFailed => "doctor detected failing checks".to_string(),
            Self::DryRunNotImplemented => "ingest --dry-run is not implemented yet".to_string(),
            Self::IngestSourceFilterNotImplemented => {
                "ingest --source is not implemented yet".to_string()
            }
            Self::ReindexAbortInvalidJobId { raw } => {
                format!("reindex --abort expects a positive integer job id, got: {raw}")
            }
            Self::ReindexTargetRequired => {
                "reindex requires --target unless --abort is given".to_string()
            }
            Self::ReplayArtifactNotFound { kind, key } => {
                format!("replay artifact not found: {kind}/{key}")
            }
            Self::PublishRecordNotFound { idempotency_key } => {
                format!("publish record not found: {idempotency_key}")
            }
            Self::PublishConflict { state } => {
                format!("publish record is in conflicting state: {state}")
            }
        }
    }

    pub fn command_name(&self) -> &str {
        match self {
            Self::DoctorFailed => "doctor",
            Self::DryRunNotImplemented | Self::IngestSourceFilterNotImplemented => "ingest",
            Self::ReindexAbortInvalidJobId { .. } | Self::ReindexTargetRequired => "reindex",
            Self::ReplayArtifactNotFound { .. } => "replay",
            Self::PublishRecordNotFound { .. } | Self::PublishConflict { .. } => "publish",
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
