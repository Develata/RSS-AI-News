use async_trait::async_trait;
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, StorageError, StoragePool};

#[derive(Debug, Clone)]
pub struct NewPublishRecord {
    pub idempotency_key: String,
    pub category_key: String,
    pub report_date: String,
    pub target_timezone: String,
    pub render_version: i64,
    pub selection_policy_version: i64,
    pub remote_target: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct PublishRecord {
    pub id: i64,
    pub idempotency_key: String,
    pub category_key: String,
    pub report_date: String,
    pub target_timezone: String,
    pub render_version: i64,
    pub selection_policy_version: i64,
    pub state: String,
    pub snapshot_frozen_at: Option<OffsetDateTime>,
    pub rendered_at: Option<OffsetDateTime>,
    pub local_stored_at: Option<OffsetDateTime>,
    pub remote_published_at: Option<OffsetDateTime>,
    pub local_path: Option<String>,
    pub remote_target: Option<String>,
    pub commit_sha: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub last_error_kind: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClaimedPublishRecord {
    pub id: i64,
    pub idempotency_key: String,
    pub category_key: String,
    pub report_date: String,
    pub target_timezone: String,
    pub render_version: i64,
    pub selection_policy_version: i64,
    pub state: String,
    pub remote_target: Option<String>,
    pub attempt_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishState {
    Pending,
    SnapshotFrozen,
    Rendered,
    StoredLocal,
    PublishedLocal,
    PublishedRemote,
    Failed,
}

impl PublishState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::SnapshotFrozen => "snapshot_frozen",
            Self::Rendered => "rendered",
            Self::StoredLocal => "stored_local",
            Self::PublishedLocal => "published_local",
            Self::PublishedRemote => "published_remote",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishTimestampField {
    SnapshotFrozenAt,
    RenderedAt,
    LocalStoredAt,
    RemotePublishedAt,
}

#[derive(Debug, Clone, Default)]
pub struct PublishAdvanceExtras {
    pub local_path: Option<String>,
    pub remote_target: Option<String>,
    pub commit_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAdvanceStatus {
    Advanced,
    PublishRecordConflict,
    ArticleStateConflict { article_id: i64 },
}

#[derive(Debug, Clone)]
pub struct TerminalAdvanceOutcome {
    pub status: TerminalAdvanceStatus,
}

#[async_trait]
pub trait PublishRecordRepository: Send + Sync {
    async fn create_if_new(&self, item: &NewPublishRecord) -> Result<Option<i64>, StorageError>;
    async fn find_by_id(&self, id: i64) -> Result<Option<PublishRecord>, StorageError>;
    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<PublishRecord>, StorageError>;
    async fn claim_pending_for_freeze(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError>;
    async fn claim_frozen_for_render(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError>;
    async fn claim_rendered_for_local_store(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError>;
    async fn claim_local_for_remote_publish(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError>;
    async fn release_advance(
        &self,
        id: i64,
        owner: &str,
        from: PublishState,
        to: PublishState,
        timestamp_field: PublishTimestampField,
        now: OffsetDateTime,
        extras: PublishAdvanceExtras,
    ) -> Result<bool, StorageError>;
    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError>;
    #[allow(clippy::too_many_arguments)]
    async fn release_terminal_advance_with_articles(
        &self,
        id: i64,
        owner: &str,
        from: PublishState,
        to: PublishState,
        timestamp_field: PublishTimestampField,
        promote_article_ids: Vec<i64>,
        extras: PublishAdvanceExtras,
        now: OffsetDateTime,
    ) -> Result<TerminalAdvanceOutcome, StorageError>;
}

#[derive(Debug, Clone)]
pub struct PublishRecordRepo {
    pub(super) pool: StoragePool,
}

impl PublishRecordRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    pub(super) fn sqlite_pool(&self) -> Result<&SqlitePool, StorageError> {
        self.pool.require_sqlite("publish_record_repo")
    }
}
