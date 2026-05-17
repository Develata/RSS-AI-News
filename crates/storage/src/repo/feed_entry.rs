//! feed_entries 持久化层。
//!
//! ## W11-P3-E-2：PG 分支落地
//!
//! 按 `docs/design/storage-multi-dialect.md` §6.2 模式：trait method `match`
//! 分发到 sqlite_*/pg_* 私有 helper。除 `claim_pending_fetch` 外 SQL 跨方言
//! 完全等价（EXISTS 已 P1 改 CASE WHEN decode i32）。
//!
//! **§6.4 PG 契约**：`claim_pending_fetch` 子查询必须 `FOR UPDATE SKIP LOCKED`，
//! 让 ingest 多 worker 并发 claim 同一 pending_fetch 池时各自拿到不同候选；
//! SQLite 整库写锁本身串行化，无此语法也不需要。SQL 因此分裂为
//! `CLAIM_PENDING_FETCH_SQLITE_SQL` / `CLAIM_PENDING_FETCH_PG_SQL`。

use async_trait::async_trait;
use sqlx::{FromRow, PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, StorageError, StoragePool, classify_db_error};

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
    pool: StoragePool,
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

// ── 共享 SQL ───────────────────────────────────────────────────

const INSERT_FEED_ENTRY_SQL: &str = r#"
INSERT INTO feed_entries (
    source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
    summary_raw, published_at, discovered_at, state, dedup_decision
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending_fetch', 'fresh')
ON CONFLICT(source_id, feed_entry_uid) DO NOTHING
RETURNING id
"#;

const EXISTS_BY_LINK_HASH_SQL: &str =
    "SELECT CASE WHEN EXISTS(SELECT 1 FROM feed_entries WHERE link_hash = $1) THEN 1 ELSE 0 END";

const SELECT_FEED_ENTRY_BY_ID_SQL: &str = r#"
SELECT id, source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
       summary_raw, published_at, discovered_at, state, dedup_decision,
       article_id, lease_owner, lease_expires_at, attempt_count, last_error,
       last_error_kind, created_at, updated_at
FROM feed_entries
WHERE id = $1
"#;

/// SQLite claim：子查询无 `FOR UPDATE`（语法不支持，整库写锁兜底）。
const CLAIM_PENDING_FETCH_SQLITE_SQL: &str = r#"
UPDATE feed_entries
SET state = 'fetching',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    updated_at = $3
WHERE id IN (
    SELECT id FROM feed_entries
    WHERE state = 'pending_fetch'
      AND (lease_expires_at IS NULL OR lease_expires_at < $4)
      AND attempt_count < $5
    ORDER BY discovered_at ASC
    LIMIT $6
)
RETURNING id, source_id, normalized_link, link_hash, title_raw,
          discovered_at, attempt_count
"#;

/// PG claim：§6.4 契约——子查询 `FOR UPDATE SKIP LOCKED`，让 ingest 多 worker
/// 并发 claim 同一 pending_fetch 池时各自拿到不同候选；否则会序列化等待
/// row lock，等价单 worker。
const CLAIM_PENDING_FETCH_PG_SQL: &str = r#"
UPDATE feed_entries
SET state = 'fetching',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    updated_at = $3
WHERE id IN (
    SELECT id FROM feed_entries
    WHERE state = 'pending_fetch'
      AND (lease_expires_at IS NULL OR lease_expires_at < $4)
      AND attempt_count < $5
    ORDER BY discovered_at ASC
    LIMIT $6
    FOR UPDATE SKIP LOCKED
)
RETURNING id, source_id, normalized_link, link_hash, title_raw,
          discovered_at, attempt_count
"#;

const RELEASE_SUCCESS_SQL: &str = r#"
UPDATE feed_entries
SET state = 'persisted', article_id = $1, lease_owner = NULL,
    lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL,
    updated_at = $2
WHERE id = $3 AND lease_owner = $4
"#;

const RELEASE_FEED_FAILURE_SQL: &str = r#"
UPDATE feed_entries
SET state = $1, lease_owner = NULL, lease_expires_at = NULL,
    last_error = $2, last_error_kind = $3, updated_at = $4
WHERE id = $5 AND lease_owner = $6
"#;

/// 设计 §5.5 写明 reclaim 不改 state，但 §5.1 只领取 pending_fetch。
/// 这里按 W4b 指令采用方案 A：过期 fetching/extracting 回到 pending_fetch。
const RECLAIM_FEED_ENTRY_LEASES_SQL: &str = r#"
UPDATE feed_entries
SET state = 'pending_fetch',
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE lease_expires_at IS NOT NULL
  AND lease_expires_at < $2
  AND state IN ('fetching', 'extracting')
"#;

const RELEASE_DEDUP_SKIPPED_SQL: &str = r#"
UPDATE feed_entries
SET state = 'dedup_skipped',
    dedup_decision = $1,
    article_id = $2,
    lease_owner = NULL,
    lease_expires_at = NULL,
    last_error = NULL,
    last_error_kind = NULL,
    updated_at = $3
WHERE id = $4 AND lease_owner = $5
"#;

const RELEASE_FALLBACK_PERSISTED_SQL: &str = r#"
UPDATE feed_entries
SET state = 'fallback_persisted',
    article_id = $1,
    lease_owner = NULL,
    lease_expires_at = NULL,
    last_error = NULL,
    last_error_kind = NULL,
    updated_at = $2
WHERE id = $3 AND lease_owner = $4
"#;

const COUNT_FEED_ENTRIES_IN_WINDOW_SQL: &str = r#"
SELECT COUNT(*) FROM feed_entries
WHERE ($1 IS NULL OR created_at >= $1)
  AND ($2 IS NULL OR created_at < $2)
"#;

const RESET_FAILED_IN_WINDOW_SQL: &str = r#"
UPDATE feed_entries
SET state = 'discovered',
    attempt_count = 0,
    last_error = NULL,
    last_error_kind = NULL,
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $3
WHERE state = 'failed'
  AND ($1 IS NULL OR created_at >= $1)
  AND ($2 IS NULL OR created_at < $2)
"#;

const LIST_FOR_LINK_HASH_REINDEX_SQL: &str = r#"
SELECT id, normalized_link, link_hash
FROM feed_entries
WHERE id > $1
ORDER BY id ASC
LIMIT $2
"#;

const UPDATE_LINK_HASH_SQL: &str = r#"
UPDATE feed_entries
SET link_hash = $1, updated_at = $2
WHERE id = $3
"#;

// ── trait 实现 ─────────────────────────────────────────────────

#[async_trait]
impl FeedEntryRepository for FeedEntryRepo {
    async fn insert_if_new(&self, entry: &NewFeedEntry) -> Result<Option<i64>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_insert_if_new(p, entry).await,
            StoragePool::Postgres(p) => pg_insert_if_new(p, entry).await,
        }
    }

    async fn exists_by_link_hash(&self, link_hash: &str) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_exists_by_link_hash(p, link_hash).await,
            StoragePool::Postgres(p) => pg_exists_by_link_hash(p, link_hash).await,
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<FeedEntry>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_by_id(p, id).await,
            StoragePool::Postgres(p) => pg_find_by_id(p, id).await,
        }
    }

    async fn claim_pending_fetch(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedFeedEntry>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_claim_pending_fetch(p, request).await,
            StoragePool::Postgres(p) => pg_claim_pending_fetch(p, request).await,
        }
    }

    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_release_success(p, id, owner, article_id, now).await,
            StoragePool::Postgres(p) => pg_release_success(p, id, owner, article_id, now).await,
        }
    }

    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_feed_failure(p, id, owner, error, kind, now, "pending_fetch").await
            }
            StoragePool::Postgres(p) => {
                pg_release_feed_failure(p, id, owner, error, kind, now, "pending_fetch").await
            }
        }
    }

    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_feed_failure(p, id, owner, error, kind, now, "failed").await
            }
            StoragePool::Postgres(p) => {
                pg_release_feed_failure(p, id, owner, error, kind, now, "failed").await
            }
        }
    }

    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_reclaim_expired_leases(p, now).await,
            StoragePool::Postgres(p) => pg_reclaim_expired_leases(p, now).await,
        }
    }

    async fn release_dedup_skipped(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        decision: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_dedup_skipped(p, id, owner, article_id, decision, now).await
            }
            StoragePool::Postgres(p) => {
                pg_release_dedup_skipped(p, id, owner, article_id, decision, now).await
            }
        }
    }

    async fn release_fallback_persisted(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_fallback_persisted(p, id, owner, article_id, now).await
            }
            StoragePool::Postgres(p) => {
                pg_release_fallback_persisted(p, id, owner, article_id, now).await
            }
        }
    }

    async fn reset_failed_in_window(
        &self,
        filter: &ResetFailedFilter,
    ) -> Result<ResetFailedOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_reset_failed_in_window(p, filter).await,
            StoragePool::Postgres(p) => pg_reset_failed_in_window(p, filter).await,
        }
    }

    async fn list_for_link_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<LinkHashReindexCandidate>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_list_for_link_hash_reindex(p, after_id, batch_size).await
            }
            StoragePool::Postgres(p) => {
                pg_list_for_link_hash_reindex(p, after_id, batch_size).await
            }
        }
    }

    async fn update_link_hash(&self, id: i64, new_link_hash: &str) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_update_link_hash(p, id, new_link_hash).await,
            StoragePool::Postgres(p) => pg_update_link_hash(p, id, new_link_hash).await,
        }
    }
}

// ── SQLite helper ──────────────────────────────────────────────

async fn sqlite_insert_if_new(
    pool: &SqlitePool,
    entry: &NewFeedEntry,
) -> Result<Option<i64>, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_FEED_ENTRY_SQL)
        .bind(entry.source_id)
        .bind(&entry.feed_entry_uid)
        .bind(&entry.normalized_link)
        .bind(&entry.link_hash)
        .bind(&entry.title_raw)
        .bind(&entry.summary_raw)
        .bind(entry.published_at)
        .bind(entry.discovered_at)
        .fetch_optional(pool)
        .await
        .map_err(|error| classify_insert_error(error, entry))
}

async fn sqlite_exists_by_link_hash(
    pool: &SqlitePool,
    link_hash: &str,
) -> Result<bool, StorageError> {
    let exists = sqlx::query_scalar::<_, i32>(EXISTS_BY_LINK_HASH_SQL)
        .bind(link_hash)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(exists != 0)
}

async fn sqlite_find_by_id(pool: &SqlitePool, id: i64) -> Result<Option<FeedEntry>, StorageError> {
    sqlx::query_as::<_, FeedEntry>(SELECT_FEED_ENTRY_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

async fn sqlite_claim_pending_fetch(
    pool: &SqlitePool,
    request: &ClaimRequest,
) -> Result<Vec<ClaimedFeedEntry>, StorageError> {
    sqlx::query_as::<_, ClaimedFeedEntry>(CLAIM_PENDING_FETCH_SQLITE_SQL)
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
        .bind(request.now)
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

async fn sqlite_release_success(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    article_id: i64,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_SUCCESS_SQL)
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn sqlite_release_feed_failure(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    now: OffsetDateTime,
    state: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_FEED_FAILURE_SQL)
        .bind(state)
        .bind(error)
        .bind(kind)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn sqlite_reclaim_expired_leases(
    pool: &SqlitePool,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(RECLAIM_FEED_ENTRY_LEASES_SQL)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

async fn sqlite_release_dedup_skipped(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    article_id: i64,
    decision: &str,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_DEDUP_SKIPPED_SQL)
        .bind(decision)
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn sqlite_release_fallback_persisted(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    article_id: i64,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_FALLBACK_PERSISTED_SQL)
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn sqlite_reset_failed_in_window(
    pool: &SqlitePool,
    filter: &ResetFailedFilter,
) -> Result<ResetFailedOutcome, StorageError> {
    let examined = sqlx::query_scalar::<_, i64>(COUNT_FEED_ENTRIES_IN_WINDOW_SQL)
        .bind(filter.date_from)
        .bind(filter.date_to)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;

    let result = sqlx::query(RESET_FAILED_IN_WINDOW_SQL)
        .bind(filter.date_from)
        .bind(filter.date_to)
        .bind(OffsetDateTime::now_utc())
        .execute(pool)
        .await
        .map_err(StorageError::from)?;

    Ok(ResetFailedOutcome {
        examined: u32::try_from(examined).unwrap_or(u32::MAX),
        reset: u32::try_from(result.rows_affected()).unwrap_or(u32::MAX),
    })
}

async fn sqlite_list_for_link_hash_reindex(
    pool: &SqlitePool,
    after_id: i64,
    batch_size: u32,
) -> Result<Vec<LinkHashReindexCandidate>, StorageError> {
    sqlx::query_as::<_, LinkHashReindexCandidate>(LIST_FOR_LINK_HASH_REINDEX_SQL)
        .bind(after_id)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

async fn sqlite_update_link_hash(
    pool: &SqlitePool,
    id: i64,
    new_link_hash: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(UPDATE_LINK_HASH_SQL)
        .bind(new_link_hash)
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

// ── PostgreSQL helper（W11-P3-E-2） ─────────────────────────────

async fn pg_insert_if_new(
    pool: &PgPool,
    entry: &NewFeedEntry,
) -> Result<Option<i64>, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_FEED_ENTRY_SQL)
        .bind(entry.source_id)
        .bind(&entry.feed_entry_uid)
        .bind(&entry.normalized_link)
        .bind(&entry.link_hash)
        .bind(&entry.title_raw)
        .bind(&entry.summary_raw)
        .bind(entry.published_at)
        .bind(entry.discovered_at)
        .fetch_optional(pool)
        .await
        .map_err(|error| classify_insert_error(error, entry))
}

async fn pg_exists_by_link_hash(pool: &PgPool, link_hash: &str) -> Result<bool, StorageError> {
    let exists = sqlx::query_scalar::<_, i32>(EXISTS_BY_LINK_HASH_SQL)
        .bind(link_hash)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(exists != 0)
}

async fn pg_find_by_id(pool: &PgPool, id: i64) -> Result<Option<FeedEntry>, StorageError> {
    sqlx::query_as::<_, FeedEntry>(SELECT_FEED_ENTRY_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

async fn pg_claim_pending_fetch(
    pool: &PgPool,
    request: &ClaimRequest,
) -> Result<Vec<ClaimedFeedEntry>, StorageError> {
    sqlx::query_as::<_, ClaimedFeedEntry>(CLAIM_PENDING_FETCH_PG_SQL)
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
        .bind(request.now)
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

async fn pg_release_success(
    pool: &PgPool,
    id: i64,
    owner: &str,
    article_id: i64,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_SUCCESS_SQL)
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn pg_release_feed_failure(
    pool: &PgPool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    now: OffsetDateTime,
    state: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_FEED_FAILURE_SQL)
        .bind(state)
        .bind(error)
        .bind(kind)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn pg_reclaim_expired_leases(
    pool: &PgPool,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(RECLAIM_FEED_ENTRY_LEASES_SQL)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

async fn pg_release_dedup_skipped(
    pool: &PgPool,
    id: i64,
    owner: &str,
    article_id: i64,
    decision: &str,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_DEDUP_SKIPPED_SQL)
        .bind(decision)
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn pg_release_fallback_persisted(
    pool: &PgPool,
    id: i64,
    owner: &str,
    article_id: i64,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_FALLBACK_PERSISTED_SQL)
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn pg_reset_failed_in_window(
    pool: &PgPool,
    filter: &ResetFailedFilter,
) -> Result<ResetFailedOutcome, StorageError> {
    let examined = sqlx::query_scalar::<_, i64>(COUNT_FEED_ENTRIES_IN_WINDOW_SQL)
        .bind(filter.date_from)
        .bind(filter.date_to)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;

    let result = sqlx::query(RESET_FAILED_IN_WINDOW_SQL)
        .bind(filter.date_from)
        .bind(filter.date_to)
        .bind(OffsetDateTime::now_utc())
        .execute(pool)
        .await
        .map_err(StorageError::from)?;

    Ok(ResetFailedOutcome {
        examined: u32::try_from(examined).unwrap_or(u32::MAX),
        reset: u32::try_from(result.rows_affected()).unwrap_or(u32::MAX),
    })
}

async fn pg_list_for_link_hash_reindex(
    pool: &PgPool,
    after_id: i64,
    batch_size: u32,
) -> Result<Vec<LinkHashReindexCandidate>, StorageError> {
    sqlx::query_as::<_, LinkHashReindexCandidate>(LIST_FOR_LINK_HASH_REINDEX_SQL)
        .bind(after_id)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

async fn pg_update_link_hash(
    pool: &PgPool,
    id: i64,
    new_link_hash: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(UPDATE_LINK_HASH_SQL)
        .bind(new_link_hash)
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

// ── helpers ────────────────────────────────────────────────────

fn classify_insert_error(error: sqlx::Error, entry: &NewFeedEntry) -> StorageError {
    classify_db_error(
        error,
        "feed_entries",
        format!("{}/{}", entry.source_id, entry.feed_entry_uid),
    )
}
