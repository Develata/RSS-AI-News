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
    /// cli-semantics §4.8 line 290: `reindex --abort <job_id>` 取消指定 job。
    /// 当前 storage 层尚未提供 `reindex_jobs` 表实现，runtime 无 abort 接口。
    /// CLI 接受 flag 以满足 §4.8 表面契约；执行层返回此错误并以 exit code 1
    /// 退出（与现有 *NotImplemented 一致）。
    #[error("reindex --abort is not implemented yet (job_id={job_id})")]
    ReindexAbortNotImplemented { job_id: String },
    /// cli-semantics §4.8 line 287: `--target` 在非 `--abort` 模式下必填。
    /// clap 通过 `required_unless_present` 保证；这是兜底，正常分支不会触发。
    #[error("reindex requires --target unless --abort is given")]
    ReindexTargetRequired,
    #[error("dry-run is not implemented for reindex yet")]
    ReindexDryRunNotImplemented,
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
            Self::ReindexAbortNotImplemented { .. } => "reindex_abort_not_implemented",
            Self::ReindexTargetRequired => "reindex_target_required",
            Self::ReindexDryRunNotImplemented => "reindex_dry_run_not_implemented",
            Self::ReplayArtifactNotFound { .. } => "replay_artifact_not_found",
            Self::PublishRecordNotFound { .. } => "publish_record_not_found",
            Self::PublishConflict { .. } => "publish_conflict",
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) => ExitCode::ConfigError,
            // §4.8 line 327: reindex 参数错误 → exit 2 (UserError)。
            // `ReindexTargetRequired` 来自 clap 没接住的 invariant，归类参数错误。
            Self::ReindexTargetRequired => ExitCode::UserError,
            Self::Runtime(_)
            | Self::Io(_)
            | Self::Storage(_)
            | Self::DoctorFailed
            | Self::DryRunNotImplemented
            | Self::IngestSourceFilterNotImplemented
            | Self::ReindexAbortNotImplemented { .. }
            | Self::ReindexDryRunNotImplemented
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
            Self::ReindexAbortNotImplemented { job_id } => {
                format!("reindex --abort is not implemented yet (job_id={job_id})")
            }
            Self::ReindexTargetRequired => {
                "reindex requires --target unless --abort is given".to_string()
            }
            Self::ReindexDryRunNotImplemented => {
                "reindex --dry-run is not implemented yet".to_string()
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
            Self::ReindexAbortNotImplemented { .. }
            | Self::ReindexTargetRequired
            | Self::ReindexDryRunNotImplemented => "reindex",
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
