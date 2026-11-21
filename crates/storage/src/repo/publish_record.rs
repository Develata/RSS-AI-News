use async_trait::async_trait;
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, ReleaseFailureOutcome, StorageError, StoragePool};

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
    /// The timestamp fixed by the first render (rendered into
    /// `{generated_at}`); later stages re-render with it so stored, remote and
    /// rebuilt reports stay byte-identical. `None` before render.
    pub rendered_at: Option<OffsetDateTime>,
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
    /// [`Self::claim_pending_for_freeze`] limited to records of
    /// `category_key` (R1-P-S15).
    async fn claim_pending_for_freeze_in_category(
        &self,
        request: &ClaimRequest,
        category_key: &str,
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
    async fn claim_publish_by_ids(
        &self,
        request: &ClaimRequest,
        state: PublishState,
        ids: &[i64],
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError>;
    /// R3F-S15: claims `pending` record `id` only if it belongs to
    /// `category_key` — the category is part of the claim's `WHERE`, so a
    /// mismatched call never claims (or charges) another category's record.
    async fn claim_pending_for_freeze_by_id_in_category(
        &self,
        request: &ClaimRequest,
        id: i64,
        category_key: &str,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError>;
    async fn claim_local_for_remote_publish_by_ids(
        &self,
        request: &ClaimRequest,
        ids: &[i64],
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
    /// W15 §3 折叠：retryable 失败按 `stage_attempt_count >= max_attempts` 在 SQL 内
    /// 决定维持原阶段态 / 转 `failed`。`last_error*` 写真实底层错误。
    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError> {
        self.release_retryable_failure_until(id, owner, error, kind, max_attempts, None, now)
            .await
    }
    /// [`Self::release_retryable_failure`] that also persists the earliest
    /// time the record may be claimed again (`next_attempt_at`, R1-P-S12):
    /// `Some` after a rate limit, `None` clears any earlier wait.
    #[allow(clippy::too_many_arguments)]
    async fn release_retryable_failure_until(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        retry_not_before: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError>;
    /// Releases a `pending` record whose freeze found no candidates back to
    /// claimable `pending`, refunding the claim's attempt. Returns `false` when
    /// the lease was lost.
    async fn release_empty_snapshot(
        &self,
        id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    /// Releases a `pending` record whose freeze lost an article (ownership or
    /// state conflict) back to claimable `pending`, refunding the claim.
    /// Returns `false` when the lease was lost.
    async fn release_freeze_conflict(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    /// Releases a record claimed in `state` whose snapshot shares an article
    /// with a winning snapshot (P-B1): lease cleared, claim refunded, state
    /// kept, `last_error*` set. Returns `false` when the lease was lost.
    async fn release_ownership_conflict(
        &self,
        id: i64,
        owner: &str,
        state: PublishState,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    /// Releases a remote attempt without charging the stage (R3F-B2 / S12):
    /// lease cleared, claim refunded on both counters, `state` kept,
    /// `last_error*` set and `next_attempt_at = retry_not_before` (`None`
    /// clears a wait; SQLite stores it rounded up to the millisecond like
    /// every deadline). Returns `false` when the lease was lost.
    #[allow(clippy::too_many_arguments)]
    async fn release_deferred_retry(
        &self,
        id: i64,
        owner: &str,
        state: PublishState,
        error: &str,
        kind: &str,
        retry_not_before: Option<OffsetDateTime>,
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
    /// W15 §4 sweep：阶段态（pending/snapshot_frozen/rendered/stored_local）+
    /// `stage_attempt_count >= max_attempts` + lease 空/过期 → `failed`。预算按
    /// 阶段计（ADR 0012）。保留既有 `last_error*`。返回转终态的行数。
    async fn terminalize_exhausted(
        &self,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError>;
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

    /// W11-P3-C-4：PG 入口；旧 `new(SqlitePool)` thin wrapper 保留兼容。
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }

    pub(super) fn storage_pool(&self) -> &StoragePool {
        &self.pool
    }
}
