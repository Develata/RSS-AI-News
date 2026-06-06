//! [`FeedSourceRepository`] trait 实装。
//!
//! W11-P3-C-1：每方法按 backend `match &self.pool` 分发到 sqlite_*/pg_* 私有
//! helper；SQL const 集中在 [`super::feed_source_sql`]，100% 跨方言等价。
//! `Transaction<Postgres>` 与 `Transaction<Sqlite>` 不共享 lifetime/Database
//! 关联类型，故无法用泛型抽出 helper，sqlite_*/pg_* 只在 sqlx 类型签名分叉。

use async_trait::async_trait;
use rss_ai_news_domain::{
    model::FeedSource,
    state::{FeedKind, FeedSourceStatus},
};
use sqlx::{FromRow, PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, StoragePool, classify_db_error};

use super::feed_source::{FeedSourceRepo, FeedSourceRepository, LeaseGuardedWriteOutcome};
use super::feed_source_sql::{
    LEASE_GUARD_UPDATE_REINDEX_JOBS_SQL, LIST_FEED_SOURCES_ALL_SQL,
    LIST_FEED_SOURCES_BY_CATEGORY_SQL, MARK_FEED_SOURCE_ARCHIVED_SQL, SELECT_FEED_SOURCE_BY_ID_SQL,
    SELECT_FEED_SOURCE_BY_KEYS_SQL, UPDATE_FEED_SOURCE_AFTER_FETCH_FAILURE_SQL,
    UPDATE_FEED_SOURCE_AFTER_FETCH_SUCCESS_SQL, UPSERT_FEED_SOURCE_RETURNING_ID_SQL,
    UPSERT_FEED_SOURCE_SQL,
};

// ── trait 实现：按 backend 分发到 sqlite_* / pg_* helper ──

#[async_trait]
impl FeedSourceRepository for FeedSourceRepo {
    async fn upsert(&self, src: &FeedSource) -> Result<i64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_upsert(p, src).await,
            StoragePool::Postgres(p) => pg_upsert(p, src).await,
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<FeedSource>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_by_id(p, id).await,
            StoragePool::Postgres(p) => pg_find_by_id(p, id).await,
        }
    }

    async fn find_by_keys(
        &self,
        category_key: &str,
        source_key: &str,
    ) -> Result<Option<FeedSource>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_by_keys(p, category_key, source_key).await,
            StoragePool::Postgres(p) => pg_find_by_keys(p, category_key, source_key).await,
        }
    }

    async fn list_by_category(&self, category_key: &str) -> Result<Vec<FeedSource>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_list_by_category(p, category_key).await,
            StoragePool::Postgres(p) => pg_list_by_category(p, category_key).await,
        }
    }

    async fn list_all(&self) -> Result<Vec<FeedSource>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_list_all(p).await,
            StoragePool::Postgres(p) => pg_list_all(p).await,
        }
    }

    async fn mark_archived(&self, id: i64) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_mark_archived(p, id).await,
            StoragePool::Postgres(p) => pg_mark_archived(p, id).await,
        }
    }

    async fn upsert_with_lease_guard(
        &self,
        src: &FeedSource,
        job_id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<LeaseGuardedWriteOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_upsert_with_lease_guard(p, src, job_id, owner, now).await
            }
            StoragePool::Postgres(p) => {
                pg_upsert_with_lease_guard(p, src, job_id, owner, now).await
            }
        }
    }

    async fn mark_archived_with_lease_guard(
        &self,
        id: i64,
        job_id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<LeaseGuardedWriteOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_mark_archived_with_lease_guard(p, id, job_id, owner, now).await
            }
            StoragePool::Postgres(p) => {
                pg_mark_archived_with_lease_guard(p, id, job_id, owner, now).await
            }
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
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_update_after_fetch_success(
                    p,
                    id,
                    etag,
                    last_modified,
                    fetched_at,
                    success_at,
                )
                .await
            }
            StoragePool::Postgres(p) => {
                pg_update_after_fetch_success(p, id, etag, last_modified, fetched_at, success_at)
                    .await
            }
        }
    }

    async fn update_after_fetch_failure(
        &self,
        id: i64,
        fetched_at: OffsetDateTime,
        error_msg: &str,
        error_kind: &str,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_update_after_fetch_failure(p, id, fetched_at, error_msg, error_kind).await
            }
            StoragePool::Postgres(p) => {
                pg_update_after_fetch_failure(p, id, fetched_at, error_msg, error_kind).await
            }
        }
    }
}

// ── SQLite helper（保留 P3-C-0 前的行为） ──────────────────────

async fn sqlite_upsert(pool: &SqlitePool, src: &FeedSource) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(UPSERT_FEED_SOURCE_RETURNING_ID_SQL)
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
        .map_err(|error| classify_upsert_error(error, src))
}

async fn sqlite_find_by_id(pool: &SqlitePool, id: i64) -> Result<Option<FeedSource>, StorageError> {
    let row = sqlx::query_as::<_, FeedSourceRow>(SELECT_FEED_SOURCE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(FeedSource::try_from).transpose()
}

async fn sqlite_find_by_keys(
    pool: &SqlitePool,
    category_key: &str,
    source_key: &str,
) -> Result<Option<FeedSource>, StorageError> {
    let row = sqlx::query_as::<_, FeedSourceRow>(SELECT_FEED_SOURCE_BY_KEYS_SQL)
        .bind(category_key)
        .bind(source_key)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(FeedSource::try_from).transpose()
}

async fn sqlite_list_by_category(
    pool: &SqlitePool,
    category_key: &str,
) -> Result<Vec<FeedSource>, StorageError> {
    let rows = sqlx::query_as::<_, FeedSourceRow>(LIST_FEED_SOURCES_BY_CATEGORY_SQL)
        .bind(category_key)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)?;
    rows.into_iter().map(FeedSource::try_from).collect()
}

async fn sqlite_list_all(pool: &SqlitePool) -> Result<Vec<FeedSource>, StorageError> {
    let rows = sqlx::query_as::<_, FeedSourceRow>(LIST_FEED_SOURCES_ALL_SQL)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)?;
    rows.into_iter().map(FeedSource::try_from).collect()
}

async fn sqlite_mark_archived(pool: &SqlitePool, id: i64) -> Result<bool, StorageError> {
    let result = sqlx::query(MARK_FEED_SOURCE_ARCHIVED_SQL)
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn sqlite_upsert_with_lease_guard(
    pool: &SqlitePool,
    src: &FeedSource,
    job_id: i64,
    owner: &str,
    now: OffsetDateTime,
) -> Result<LeaseGuardedWriteOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;

    let lease = sqlx::query(LEASE_GUARD_UPDATE_REINDEX_JOBS_SQL)
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

    sqlx::query(UPSERT_FEED_SOURCE_SQL)
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
        .map_err(|error| classify_upsert_error(error, src))?;

    tx.commit().await.map_err(StorageError::from)?;
    Ok(LeaseGuardedWriteOutcome::Applied)
}

async fn sqlite_mark_archived_with_lease_guard(
    pool: &SqlitePool,
    id: i64,
    job_id: i64,
    owner: &str,
    now: OffsetDateTime,
) -> Result<LeaseGuardedWriteOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;

    let lease = sqlx::query(LEASE_GUARD_UPDATE_REINDEX_JOBS_SQL)
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

    let archived = sqlx::query(MARK_FEED_SOURCE_ARCHIVED_SQL)
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

async fn sqlite_update_after_fetch_success(
    pool: &SqlitePool,
    id: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
    fetched_at: OffsetDateTime,
    success_at: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(UPDATE_FEED_SOURCE_AFTER_FETCH_SUCCESS_SQL)
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

async fn sqlite_update_after_fetch_failure(
    pool: &SqlitePool,
    id: i64,
    fetched_at: OffsetDateTime,
    error_msg: &str,
    error_kind: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(UPDATE_FEED_SOURCE_AFTER_FETCH_FAILURE_SQL)
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

// ── PostgreSQL helper（W11-P3-C-1） ─────────────────────────────
//
// SQL 100% 与 SQLite 同字符串；只在 sqlx 类型签名上分叉。`Transaction<Postgres>`
// 与 `Transaction<Sqlite>` 不共享 lifetime/Database 关联类型，故无法用泛型
// 抽出 helper。

async fn pg_upsert(pool: &PgPool, src: &FeedSource) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(UPSERT_FEED_SOURCE_RETURNING_ID_SQL)
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
        .map_err(|error| classify_upsert_error(error, src))
}

async fn pg_find_by_id(pool: &PgPool, id: i64) -> Result<Option<FeedSource>, StorageError> {
    let row = sqlx::query_as::<_, FeedSourceRow>(SELECT_FEED_SOURCE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(FeedSource::try_from).transpose()
}

async fn pg_find_by_keys(
    pool: &PgPool,
    category_key: &str,
    source_key: &str,
) -> Result<Option<FeedSource>, StorageError> {
    let row = sqlx::query_as::<_, FeedSourceRow>(SELECT_FEED_SOURCE_BY_KEYS_SQL)
        .bind(category_key)
        .bind(source_key)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(FeedSource::try_from).transpose()
}

async fn pg_list_by_category(
    pool: &PgPool,
    category_key: &str,
) -> Result<Vec<FeedSource>, StorageError> {
    let rows = sqlx::query_as::<_, FeedSourceRow>(LIST_FEED_SOURCES_BY_CATEGORY_SQL)
        .bind(category_key)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)?;
    rows.into_iter().map(FeedSource::try_from).collect()
}

async fn pg_list_all(pool: &PgPool) -> Result<Vec<FeedSource>, StorageError> {
    let rows = sqlx::query_as::<_, FeedSourceRow>(LIST_FEED_SOURCES_ALL_SQL)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)?;
    rows.into_iter().map(FeedSource::try_from).collect()
}

async fn pg_mark_archived(pool: &PgPool, id: i64) -> Result<bool, StorageError> {
    let result = sqlx::query(MARK_FEED_SOURCE_ARCHIVED_SQL)
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn pg_upsert_with_lease_guard(
    pool: &PgPool,
    src: &FeedSource,
    job_id: i64,
    owner: &str,
    now: OffsetDateTime,
) -> Result<LeaseGuardedWriteOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;

    let lease = sqlx::query(LEASE_GUARD_UPDATE_REINDEX_JOBS_SQL)
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

    sqlx::query(UPSERT_FEED_SOURCE_SQL)
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
        .map_err(|error| classify_upsert_error(error, src))?;

    tx.commit().await.map_err(StorageError::from)?;
    Ok(LeaseGuardedWriteOutcome::Applied)
}

async fn pg_mark_archived_with_lease_guard(
    pool: &PgPool,
    id: i64,
    job_id: i64,
    owner: &str,
    now: OffsetDateTime,
) -> Result<LeaseGuardedWriteOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;

    let lease = sqlx::query(LEASE_GUARD_UPDATE_REINDEX_JOBS_SQL)
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

    let archived = sqlx::query(MARK_FEED_SOURCE_ARCHIVED_SQL)
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

async fn pg_update_after_fetch_success(
    pool: &PgPool,
    id: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
    fetched_at: OffsetDateTime,
    success_at: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(UPDATE_FEED_SOURCE_AFTER_FETCH_SUCCESS_SQL)
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

async fn pg_update_after_fetch_failure(
    pool: &PgPool,
    id: i64,
    fetched_at: OffsetDateTime,
    error_msg: &str,
    error_kind: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(UPDATE_FEED_SOURCE_AFTER_FETCH_FAILURE_SQL)
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

// ── shared helpers ──────────────────────────────────────────────

fn classify_upsert_error(error: sqlx::Error, src: &FeedSource) -> StorageError {
    classify_db_error(
        error,
        "feed_sources",
        format!("{}/{}", src.category_key, src.source_key),
    )
}

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
