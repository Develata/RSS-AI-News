//! feed_sources 持久化层（契约）。
//!
//! 按 `docs-backup/design/storage-multi-dialect.md` §6.2 模式：trait method `match`
//! 分发到 sqlite_*/pg_* 私有 helper。SQL const 见 [`super::feed_source_sql`]，
//! 方言分发实装 + row 解码见 [`super::feed_source_impl`]。

use async_trait::async_trait;
use rss_ai_news_domain::model::FeedSource;
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, StoragePool};

/// F15-fix7：reindex `categories` target 写 `feed_sources` 时的 lease-guarded
/// 写入结果。`upsert_with_lease_guard` / `mark_archived_with_lease_guard`
/// 把"check reindex_jobs lease"与"写 feed_sources"放进同一事务，彻底关闭
/// fix2 残留的 guard→write TOCTOU 窗口（assert_lease_held 通过后到 upsert
/// 之间仍有可被 reclaim/abort 的间隙）。
///
/// 三态语义：
///   - `Applied`：lease 在手，feed_sources 行被真实 INSERT/UPDATE 一行
///   - `NoOp`：lease 在手，但 feed_sources 自带 WHERE 子句过滤掉了这次写
///     （仅 `mark_archived_with_lease_guard` 可能命中——目标行已经是 archived）
///   - `LeaseLost`：reindex_jobs 中 `(id, state='running', lease_owner)` 行
///     不存在；事务回滚，feed_sources 不被修改。调用方应当向上抛
///     `RuntimeError::LeaseConflict` 让 CLI 非零退出
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseGuardedWriteOutcome {
    Applied,
    NoOp,
    LeaseLost,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct RecentFeedSourceHealth {
    pub source_key: String,
    pub priority: i64,
    pub last_fetched_at: Option<OffsetDateTime>,
    pub last_success_at: Option<OffsetDateTime>,
    pub consecutive_failures: i64,
    pub last_error_kind: Option<String>,
}

#[async_trait]
pub trait RecentFeedSourceHealthRepository: Send + Sync {
    async fn list_recent_health(
        &self,
        category_key: &str,
        max_rows: u32,
    ) -> Result<Vec<RecentFeedSourceHealth>, StorageError>;
}

#[async_trait]
pub trait FeedSourceRepository: Send + Sync {
    async fn upsert(&self, src: &FeedSource) -> Result<i64, StorageError>;
    async fn find_by_id(&self, id: i64) -> Result<Option<FeedSource>, StorageError>;
    async fn find_by_keys(
        &self,
        category_key: &str,
        source_key: &str,
    ) -> Result<Option<FeedSource>, StorageError>;
    async fn list_by_category(&self, category_key: &str) -> Result<Vec<FeedSource>, StorageError>;
    async fn list_all(&self) -> Result<Vec<FeedSource>, StorageError>;
    async fn mark_archived(&self, id: i64) -> Result<bool, StorageError>;

    /// F15-fix7：reindex `categories` target 专用——把 lease 校验
    /// （reindex_jobs WHERE id=:job_id AND state='running' AND lease_owner=:owner）
    /// 与 feed_sources 的 upsert 包在同一 sqlx transaction 里，彻底关闭
    /// fix2 残留的 TOCTOU 窗口。lease 校验失败时整段事务回滚，feed_sources
    /// 不被修改，返 `LeaseLost`——调用方上抛 `RuntimeError::LeaseConflict`。
    ///
    /// 与 `upsert` 相比的语义边界：本方法**不**返回 id（runtime 端不需要），
    /// 节省一次 RETURNING；INSERT/UPDATE 冲突仍由 `classify_db_error`
    /// 映射为 `StorageError::Conflict`。
    ///
    /// **F15-fix9**：`now` 显式从调用方传入，把 `reindex_jobs.updated_at`
    /// 刷成"本次写入实际发生的瞬时时间"，与 `assert_lease_held` 的 heartbeat
    /// 语义对齐——长 categories 循环内每次写都让 reclaim 巡检看到 worker 仍
    /// 活跃。这与 `src.updated_at`（业务层的 batch 起始时间）解耦。
    async fn upsert_with_lease_guard(
        &self,
        src: &FeedSource,
        job_id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<LeaseGuardedWriteOutcome, StorageError>;

    /// F15-fix7：同上，封装 `mark_archived`。`status='archived'` 是终态，
    /// 已是 archived 的行返 `NoOp`（与原 `mark_archived` 返 `false` 等价），
    /// 让 runtime 把 `summary.archived` 仅在 `Applied` 时递增。
    async fn mark_archived_with_lease_guard(
        &self,
        id: i64,
        job_id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<LeaseGuardedWriteOutcome, StorageError>;
    async fn update_after_fetch_success(
        &self,
        id: i64,
        etag: Option<&str>,
        last_modified: Option<&str>,
        fetched_at: OffsetDateTime,
        success_at: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn update_after_fetch_failure(
        &self,
        id: i64,
        fetched_at: OffsetDateTime,
        error_msg: &str,
        error_kind: &str,
    ) -> Result<bool, StorageError>;
}

#[derive(Debug, Clone)]
pub struct FeedSourceRepo {
    pub(super) pool: StoragePool,
}

impl FeedSourceRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    /// W11-P3-C-1：直接接受 [`StoragePool`]——`StoragePool::Postgres` 也走
    /// 这条入口。`new`（接 `SqlitePool`）作为旧调用点的兼容 thin wrapper 保留，
    /// 但不再是 PG 路径的唯一入口。
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }
}
