//! feed_entries 持久化层（契约）。
//!
//! ## W11-P3-E-2：PG 分支落地
//!
//! 按 `docs/design/storage-multi-dialect.md` §6.2 模式：trait method `match`
//! 分发到 sqlite_*/pg_* 私有 helper。除 `claim_pending_fetch` 外 SQL 跨方言
//! 完全等价（EXISTS 已 P1 改 CASE WHEN decode i32）。SQL const 见
//! [`super::feed_entry_sql`]，方言分发实装见 [`super::feed_entry_impl`]。
//!
//! **§6.4 PG 契约**：`claim_pending_fetch` 子查询必须 `FOR UPDATE SKIP LOCKED`，
//! 让 ingest 多 worker 并发 claim 同一 pending_fetch 池时各自拿到不同候选；
//! SQLite 整库写锁本身串行化，无此语法也不需要。SQL 因此分裂为
//! `CLAIM_PENDING_FETCH_SQLITE_SQL` / `CLAIM_PENDING_FETCH_PG_SQL`。

use async_trait::async_trait;
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, ReleaseFailureOutcome, StorageError, StoragePool};

#[derive(Debug, Clone)]
pub struct NewFeedEntry {
    pub source_id: i64,
    pub feed_entry_uid: String,
    pub normalized_link: String,
    pub link_hash: String,
    pub title_raw: String,
    pub summary_raw: Option<String>,
    pub published_at: Option<OffsetDateTime>,
    pub discovered_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct FeedEntry {
    pub id: i64,
    pub source_id: i64,
    pub feed_entry_uid: String,
    pub normalized_link: String,
    pub link_hash: String,
    pub title_raw: String,
    pub summary_raw: Option<String>,
    pub published_at: Option<OffsetDateTime>,
    pub discovered_at: OffsetDateTime,
    pub state: String,
    pub dedup_decision: Option<String>,
    pub article_id: Option<i64>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub last_error_kind: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClaimedFeedEntry {
    pub id: i64,
    pub source_id: i64,
    pub normalized_link: String,
    pub link_hash: String,
    pub title_raw: String,
    pub discovered_at: OffsetDateTime,
    pub attempt_count: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ResetFailedFilter {
    pub date_from: Option<OffsetDateTime>,
    pub date_to: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Default)]
pub struct ResetFailedOutcome {
    pub examined: u32,
    pub reset: u32,
}

#[derive(Debug, Clone, FromRow)]
pub struct LinkHashReindexCandidate {
    pub id: i64,
    pub normalized_link: String,
    pub link_hash: String,
}

#[async_trait]
pub trait FeedEntryRepository: Send + Sync {
    async fn insert_if_new(&self, entry: &NewFeedEntry) -> Result<Option<i64>, StorageError>;
    async fn exists_by_link_hash(&self, link_hash: &str) -> Result<bool, StorageError>;
    async fn find_by_id(&self, id: i64) -> Result<Option<FeedEntry>, StorageError>;
    async fn claim_pending_fetch(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedFeedEntry>, StorageError>;
    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    /// W15 §3 折叠：retryable 失败按 `attempt_count >= max_attempts` 在 SQL 内
    /// 决定回 `pending_fetch` / 转 `failed`。`last_error*` 写真实底层错误。
    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError>;
    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError>;
    /// W15 §4 sweep：`pending_fetch` + `attempt_count >= max_attempts` + lease
    /// 空/过期 → `failed`。保留既有 `last_error*`。返回转终态的行数。
    async fn terminalize_exhausted(
        &self,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError>;
    async fn release_dedup_skipped(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        decision: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn release_fallback_persisted(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn reset_failed_in_window(
        &self,
        filter: &ResetFailedFilter,
    ) -> Result<ResetFailedOutcome, StorageError>;
    async fn list_for_link_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<LinkHashReindexCandidate>, StorageError>;
    async fn update_link_hash(&self, id: i64, new_link_hash: &str) -> Result<bool, StorageError>;
}

#[derive(Debug, Clone)]
pub struct FeedEntryRepo {
    pub(super) pool: StoragePool,
}

impl FeedEntryRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    /// W11-P3-E-2：PG 入口；旧 `new(SqlitePool)` thin wrapper 保留兼容。
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }
}
