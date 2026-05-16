use async_trait::async_trait;
use rss_ai_news_domain::{
    model::FeedSource,
    state::{FeedKind, FeedSourceStatus},
};
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, StoragePool, classify_sqlite_error};

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
    /// 节省一次 RETURNING；INSERT/UPDATE 冲突仍由 `classify_sqlite_error`
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
    pool: StoragePool,
}

impl FeedSourceRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    fn sqlite_pool(&self) -> Result<&SqlitePool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => Ok(p),
            StoragePool::Postgres(_) => Err(StorageError::UnsupportedBackend(
                "feed_source_repo postgres path is P3+".into(),
            )),
        }
    }
}

#[async_trait]
impl FeedSourceRepository for FeedSourceRepo {
    async fn upsert(&self, src: &FeedSource) -> Result<i64, StorageError> {
        let pool = self.sqlite_pool()?;
        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO feed_sources (
                category_key, source_key, display_name, feed_url, feed_kind, status,
                priority, etag, last_modified, last_fetched_at, last_success_at,
                consecutive_failures, last_error, last_error_kind, config_version,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
            ON CONFLICT(category_key, source_key) DO UPDATE SET
                display_name = excluded.display_name,
                feed_url = excluded.feed_url,
                feed_kind = excluded.feed_kind,
                status = excluded.status,
                priority = excluded.priority,
                etag = excluded.etag,
                last_modified = excluded.last_modified,
                last_fetched_at = excluded.last_fetched_at,
                last_success_at = excluded.last_success_at,
                consecutive_failures = excluded.consecutive_failures,
                last_error = excluded.last_error,
                last_error_kind = excluded.last_error_kind,
                config_version = excluded.config_version,
                updated_at = excluded.updated_at
            RETURNING id
            "#,
        )
        .bind(&src.category_key)
        .bind(&src.source_key)
        .bind(&src.display_name)
        .bind(&src.feed_url)
        .bind(feed_kind_to_str(src.feed_kind))
        .bind(feed_source_status_to_str(src.status))
        .bind(src.priority)
        .bind(&src.etag)
        .bind(&src.last_modified)
        .bind(src.last_fetched_at)
        .bind(src.last_success_at)
        .bind(src.consecutive_failures)
        .bind(&src.last_error)
        .bind(&src.last_error_kind)
        .bind(src.config_version)
        .bind(src.created_at)
        .bind(src.updated_at)
        .fetch_one(pool)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "feed_sources",
                format!("{}/{}", src.category_key, src.source_key),
            )
        })?;

        Ok(id)
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<FeedSource>, StorageError> {
        let pool = self.sqlite_pool()?;
        let row = sqlx::query_as::<_, FeedSourceRow>(SELECT_FEED_SOURCE_BY_ID)
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(StorageError::from)?;

        row.map(FeedSource::try_from).transpose()
    }

    async fn find_by_keys(
        &self,
        category_key: &str,
        source_key: &str,
    ) -> Result<Option<FeedSource>, StorageError> {
        let pool = self.sqlite_pool()?;
        let row = sqlx::query_as::<_, FeedSourceRow>(SELECT_FEED_SOURCE_BY_KEYS)
            .bind(category_key)
            .bind(source_key)
            .fetch_optional(pool)
            .await
            .map_err(StorageError::from)?;

        row.map(FeedSource::try_from).transpose()
    }

    async fn list_by_category(&self, category_key: &str) -> Result<Vec<FeedSource>, StorageError> {
        let pool = self.sqlite_pool()?;
        let rows = sqlx::query_as::<_, FeedSourceRow>(
            r#"
            SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
                   priority, etag, last_modified, last_fetched_at, last_success_at,
                   consecutive_failures, last_error, last_error_kind, config_version,
                   created_at, updated_at
            FROM feed_sources
            WHERE category_key = $1 AND status = 'active'
            ORDER BY priority ASC, source_key ASC
            "#,
        )
        .bind(category_key)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)?;

        rows.into_iter().map(FeedSource::try_from).collect()
    }

    async fn list_all(&self) -> Result<Vec<FeedSource>, StorageError> {
        let pool = self.sqlite_pool()?;
        let rows = sqlx::query_as::<_, FeedSourceRow>(
            r#"
            SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
                   priority, etag, last_modified, last_fetched_at, last_success_at,
                   consecutive_failures, last_error, last_error_kind, config_version,
                   created_at, updated_at
            FROM feed_sources
            ORDER BY id ASC
            "#,
        )
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)?;

        rows.into_iter().map(FeedSource::try_from).collect()
    }

    async fn mark_archived(&self, id: i64) -> Result<bool, StorageError> {
        let pool = self.sqlite_pool()?;
        let result = sqlx::query(
            r#"
            UPDATE feed_sources
            SET status = 'archived', updated_at = $1
            WHERE id = $2 AND status <> 'archived'
            "#,
        )
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;

        Ok(result.rows_affected() == 1)
    }

    async fn upsert_with_lease_guard(
        &self,
        src: &FeedSource,
        job_id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<LeaseGuardedWriteOutcome, StorageError> {
        let pool = self.sqlite_pool()?;
        let mut tx = pool.begin().await.map_err(StorageError::from)?;

        // 1) lease guard：与 `ReindexJobRepository::assert_lease_held` 同语义
        //    的 UPDATE——rows_affected 充当谓词，顺手把 reindex_jobs.updated_at
        //    刷新到调用方传入的 `now`（fix9：与 src.updated_at 解耦，让 reclaim
        //    巡检的 heartbeat 跟随实际写入瞬时）。lease 失效（state≠'running'
        //    或 owner 不匹配）时 rows_affected == 0，整段回滚 → LeaseLost。
        let lease = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET updated_at = $1
            WHERE id = $2 AND state = 'running' AND lease_owner = $3
            "#,
        )
        .bind(now)
        .bind(job_id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        if lease.rows_affected() != 1 {
            tx.rollback().await.map_err(StorageError::from)?;
            return Ok(LeaseGuardedWriteOutcome::LeaseLost);
        }

        // 2) feed_sources upsert（与 `upsert` 完全相同的 SQL；不复用是因为
        //    那一版直接走 self.pool，本路径要在 tx 上执行）。INSERT/UPDATE
        //    冲突保留 classify_sqlite_error 映射；本方法不返回 id。
        sqlx::query(
            r#"
            INSERT INTO feed_sources (
                category_key, source_key, display_name, feed_url, feed_kind, status,
                priority, etag, last_modified, last_fetched_at, last_success_at,
                consecutive_failures, last_error, last_error_kind, config_version,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
            ON CONFLICT(category_key, source_key) DO UPDATE SET
                display_name = excluded.display_name,
                feed_url = excluded.feed_url,
                feed_kind = excluded.feed_kind,
                status = excluded.status,
                priority = excluded.priority,
                etag = excluded.etag,
                last_modified = excluded.last_modified,
                last_fetched_at = excluded.last_fetched_at,
                last_success_at = excluded.last_success_at,
                consecutive_failures = excluded.consecutive_failures,
                last_error = excluded.last_error,
                last_error_kind = excluded.last_error_kind,
                config_version = excluded.config_version,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&src.category_key)
        .bind(&src.source_key)
        .bind(&src.display_name)
        .bind(&src.feed_url)
        .bind(feed_kind_to_str(src.feed_kind))
        .bind(feed_source_status_to_str(src.status))
        .bind(src.priority)
        .bind(&src.etag)
        .bind(&src.last_modified)
        .bind(src.last_fetched_at)
        .bind(src.last_success_at)
        .bind(src.consecutive_failures)
        .bind(&src.last_error)
        .bind(&src.last_error_kind)
        .bind(src.config_version)
        .bind(src.created_at)
        .bind(src.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "feed_sources",
                format!("{}/{}", src.category_key, src.source_key),
            )
        })?;

        tx.commit().await.map_err(StorageError::from)?;
        Ok(LeaseGuardedWriteOutcome::Applied)
    }

    async fn mark_archived_with_lease_guard(
        &self,
        id: i64,
        job_id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<LeaseGuardedWriteOutcome, StorageError> {
        let pool = self.sqlite_pool()?;
        let mut tx = pool.begin().await.map_err(StorageError::from)?;

        let lease = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET updated_at = $1
            WHERE id = $2 AND state = 'running' AND lease_owner = $3
            "#,
        )
        .bind(now)
        .bind(job_id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        if lease.rows_affected() != 1 {
            tx.rollback().await.map_err(StorageError::from)?;
            return Ok(LeaseGuardedWriteOutcome::LeaseLost);
        }

        let archived = sqlx::query(
            r#"
            UPDATE feed_sources
            SET status = 'archived', updated_at = $1
            WHERE id = $2 AND status <> 'archived'
            "#,
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;

        tx.commit().await.map_err(StorageError::from)?;
        if archived.rows_affected() == 1 {
            Ok(LeaseGuardedWriteOutcome::Applied)
        } else {
            Ok(LeaseGuardedWriteOutcome::NoOp)
        }
    }

    async fn update_after_fetch_success(
        &self,
        id: i64,
        etag: Option<&str>,
        last_modified: Option<&str>,
        fetched_at: OffsetDateTime,
        success_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let pool = self.sqlite_pool()?;
        let result = sqlx::query(
            r#"
            UPDATE feed_sources
            SET etag = $1,
                last_modified = $2,
                last_fetched_at = $3,
                last_success_at = $4,
                consecutive_failures = 0,
                last_error = NULL,
                last_error_kind = NULL,
                updated_at = $5
            WHERE id = $6
            "#,
        )
        .bind(etag)
        .bind(last_modified)
        .bind(fetched_at)
        .bind(success_at)
        .bind(fetched_at)
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;

        Ok(result.rows_affected() == 1)
    }

    async fn update_after_fetch_failure(
        &self,
        id: i64,
        fetched_at: OffsetDateTime,
        error_msg: &str,
        error_kind: &str,
    ) -> Result<bool, StorageError> {
        let pool = self.sqlite_pool()?;
        let result = sqlx::query(
            r#"
            UPDATE feed_sources
            SET last_fetched_at = $1,
                consecutive_failures = consecutive_failures + 1,
                last_error = $2,
                last_error_kind = $3,
                updated_at = $4
            WHERE id = $5
            "#,
        )
        .bind(fetched_at)
        .bind(error_msg)
        .bind(error_kind)
        .bind(fetched_at)
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;

        Ok(result.rows_affected() == 1)
    }
}

const SELECT_FEED_SOURCE_BY_ID: &str = r#"
SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
       priority, etag, last_modified, last_fetched_at, last_success_at,
       consecutive_failures, last_error, last_error_kind, config_version,
       created_at, updated_at
FROM feed_sources
WHERE id = $1
"#;

const SELECT_FEED_SOURCE_BY_KEYS: &str = r#"
SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
       priority, etag, last_modified, last_fetched_at, last_success_at,
       consecutive_failures, last_error, last_error_kind, config_version,
       created_at, updated_at
FROM feed_sources
WHERE category_key = $1 AND source_key = $2
"#;

#[derive(Debug, FromRow)]
struct FeedSourceRow {
    id: i64,
    category_key: String,
    source_key: String,
    display_name: String,
    feed_url: String,
    feed_kind: String,
    status: String,
    priority: i64,
    etag: Option<String>,
    last_modified: Option<String>,
    last_fetched_at: Option<OffsetDateTime>,
    last_success_at: Option<OffsetDateTime>,
    consecutive_failures: i64,
    last_error: Option<String>,
    last_error_kind: Option<String>,
    config_version: i64,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<FeedSourceRow> for FeedSource {
    type Error = StorageError;

    fn try_from(row: FeedSourceRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            category_key: row.category_key,
            source_key: row.source_key,
            display_name: row.display_name,
            feed_url: row.feed_url,
            feed_kind: parse_feed_kind(&row.feed_kind)?,
            status: parse_feed_source_status(&row.status)?,
            priority: row.priority,
            etag: row.etag,
            last_modified: row.last_modified,
            last_fetched_at: row.last_fetched_at,
            last_success_at: row.last_success_at,
            consecutive_failures: row.consecutive_failures,
            last_error: row.last_error,
            last_error_kind: row.last_error_kind,
            config_version: row.config_version,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

fn feed_kind_to_str(kind: FeedKind) -> &'static str {
    match kind {
        FeedKind::Rss => "rss",
        FeedKind::Atom => "atom",
        FeedKind::JsonFeed => "json_feed",
        FeedKind::RssHub => "rss_hub",
    }
}

fn parse_feed_kind(value: &str) -> Result<FeedKind, StorageError> {
    match value {
        "rss" => Ok(FeedKind::Rss),
        "atom" => Ok(FeedKind::Atom),
        "json_feed" => Ok(FeedKind::JsonFeed),
        "rss_hub" | "rsshub" => Ok(FeedKind::RssHub),
        other => Err(StorageError::Corruption(format!(
            "invalid feed_kind: {other}"
        ))),
    }
}

fn feed_source_status_to_str(status: FeedSourceStatus) -> &'static str {
    match status {
        FeedSourceStatus::Active => "active",
        FeedSourceStatus::Paused => "paused",
        FeedSourceStatus::Archived => "archived",
    }
}

fn parse_feed_source_status(value: &str) -> Result<FeedSourceStatus, StorageError> {
    match value {
        "active" => Ok(FeedSourceStatus::Active),
        "paused" => Ok(FeedSourceStatus::Paused),
        "archived" => Ok(FeedSourceStatus::Archived),
        other => Err(StorageError::Corruption(format!(
            "invalid feed source status: {other}"
        ))),
    }
}
