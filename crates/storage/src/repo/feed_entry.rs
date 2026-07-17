//! feed_entries 持久化层（契约）。
//!
//! ## W11-P3-E-2：PG 分支落地
//!
//! 按 `docs-backup/design/storage-multi-dialect.md` §6.2 模式：trait method `match`
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

/// Fixed production SQL exposed only so integration tests can run
/// `EXPLAIN QUERY PLAN` against the exact query instead of a copied surrogate.
#[doc(hidden)]
pub const RECENT_ENTRIES_SQLITE_QUERY_FOR_DIAGNOSTICS: &str =
    super::feed_entry_sql::LIST_RECENT_FEED_ENTRIES_SQLITE_SQL;

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

/// INSERT 的原子去重结果。`link_hash` 与 `(source_id, feed_entry_uid)` 都由
/// 数据库唯一约束裁决；runtime 不再用 SELECT→INSERT 预检查决定语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedEntryInsertOutcome {
    Inserted(i64),
    UidDuplicate,
    LinkDuplicate,
}

/// link_hash reindex 的写入结果。若新 hash 已被另一 canonical row 占有，
/// 当前 row 会保留但转为 shadow，避免删除历史审计/关联数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateLinkHashOutcome {
    Updated,
    ConflictShadowed,
    Missing,
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

/// 固定领域 projection 的输入。`max_rows` 由 Runtime Flow 限制为 public limit + 1；
/// repository 不接受 SQL fragment、任意排序或列选择。
#[derive(Debug, Clone)]
pub struct RecentFeedEntryFilter {
    pub category_key: String,
    pub discovered_after: OffsetDateTime,
    pub max_rows: u32,
}

/// 面向 discovery consumer 的最小 feed-entry read model。刻意不含
/// `summary_raw`、`last_error`、lease、attempt 或 article/AI/publish 数据。
#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct RecentFeedEntry {
    pub id: i64,
    pub source_key: String,
    pub source_priority: i64,
    pub title: String,
    pub url: String,
    pub published_at: Option<OffsetDateTime>,
    pub discovered_at: OffsetDateTime,
    pub state: String,
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

/// 面向只读 discovery flow 的最小 entry projection。与 [`FeedEntryRepository`]
/// 分离，使 runtime dependency 在类型上不暴露 insert/claim/release/state transition。
#[async_trait]
pub trait RecentFeedEntryRepository: Send + Sync {
    async fn list_recent(
        &self,
        filter: &RecentFeedEntryFilter,
    ) -> Result<Vec<RecentFeedEntry>, StorageError>;
}

#[async_trait]
pub trait FeedEntryRepository: Send + Sync {
    async fn insert_deduplicated(
        &self,
        entry: &NewFeedEntry,
    ) -> Result<FeedEntryInsertOutcome, StorageError>;

    /// 兼容旧调用面：任一 dedup conflict 都映射为 `None`。需要区分 UID/link
    /// 的 runtime 应调用 [`Self::insert_deduplicated`]。
    async fn insert_if_new(&self, entry: &NewFeedEntry) -> Result<Option<i64>, StorageError> {
        Ok(match self.insert_deduplicated(entry).await? {
            FeedEntryInsertOutcome::Inserted(id) => Some(id),
            FeedEntryInsertOutcome::UidDuplicate | FeedEntryInsertOutcome::LinkDuplicate => None,
        })
    }

    async fn exists_by_link_hash(&self, link_hash: &str) -> Result<bool, StorageError>;
    async fn find_by_id(&self, id: i64) -> Result<Option<FeedEntry>, StorageError>;
    async fn list_recent(
        &self,
        filter: &RecentFeedEntryFilter,
    ) -> Result<Vec<RecentFeedEntry>, StorageError>;
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
    async fn update_link_hash(
        &self,
        id: i64,
        new_link_hash: &str,
    ) -> Result<UpdateLinkHashOutcome, StorageError>;
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
