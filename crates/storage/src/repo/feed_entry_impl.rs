//! [`FeedEntryRepository`] trait 实装。
//!
//! W11-P3-E-2：每方法按 backend `match &self.pool` 分发到 sqlite_*/pg_* 私有
//! helper；SQL const 集中在 [`super::feed_entry_sql`]；`claim_pending_fetch`
//! 在 PG 加 `FOR UPDATE SKIP LOCKED`（§6.4 契约），故两侧 SQL 分叉。

use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, ReleaseFailureOutcome, StorageError, StoragePool, classify_db_error};

use super::feed_entry::{
    ClaimedFeedEntry, FeedEntry, FeedEntryRepo, FeedEntryRepository, LinkHashReindexCandidate,
    NewFeedEntry, ResetFailedFilter, ResetFailedOutcome,
};
use super::feed_entry_sql::{
    CLAIM_PENDING_FETCH_PG_SQL, CLAIM_PENDING_FETCH_SQLITE_SQL, COUNT_FEED_ENTRIES_IN_WINDOW_SQL,
    EXISTS_BY_LINK_HASH_SQL, INSERT_FEED_ENTRY_SQL, LIST_FOR_LINK_HASH_REINDEX_SQL,
    RECLAIM_FEED_ENTRY_LEASES_SQL, RELEASE_DEDUP_SKIPPED_SQL, RELEASE_FALLBACK_PERSISTED_SQL,
    RELEASE_FEED_FAILURE_SQL, RELEASE_FEED_RETRYABLE_FAILURE_SQL, RELEASE_SUCCESS_SQL,
    RESET_FAILED_IN_WINDOW_SQL, SELECT_FEED_ENTRY_BY_ID_SQL, TERMINALIZE_EXHAUSTED_FEED_SQL,
    UPDATE_LINK_HASH_SQL,
};

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
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_feed_retryable_failure(p, id, owner, error, kind, max_attempts, now)
                    .await
            }
            StoragePool::Postgres(p) => {
                pg_release_feed_retryable_failure(p, id, owner, error, kind, max_attempts, now)
                    .await
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

    async fn terminalize_exhausted(
        &self,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_terminalize_exhausted(p, max_attempts, now).await,
            StoragePool::Postgres(p) => pg_terminalize_exhausted(p, max_attempts, now).await,
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

async fn sqlite_release_feed_retryable_failure(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<ReleaseFailureOutcome, StorageError> {
    let state = sqlx::query_scalar::<_, String>(RELEASE_FEED_RETRYABLE_FAILURE_SQL)
        .bind(i64::from(max_attempts))
        .bind(error)
        .bind(kind)
        .bind(now)
        .bind(id)
        .bind(owner)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(ReleaseFailureOutcome {
        released: state.is_some(),
        exhausted: state.as_deref() == Some("failed"),
    })
}

async fn sqlite_terminalize_exhausted(
    pool: &SqlitePool,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(TERMINALIZE_EXHAUSTED_FEED_SQL)
        .bind(now)
        .bind(i64::from(max_attempts))
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

async fn pg_release_feed_retryable_failure(
    pool: &PgPool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<ReleaseFailureOutcome, StorageError> {
    let state = sqlx::query_scalar::<_, String>(RELEASE_FEED_RETRYABLE_FAILURE_SQL)
        .bind(i64::from(max_attempts))
        .bind(error)
        .bind(kind)
        .bind(now)
        .bind(id)
        .bind(owner)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(ReleaseFailureOutcome {
        released: state.is_some(),
        exhausted: state.as_deref() == Some("failed"),
    })
}

async fn pg_terminalize_exhausted(
    pool: &PgPool,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(TERMINALIZE_EXHAUSTED_FEED_SQL)
        .bind(now)
        .bind(i64::from(max_attempts))
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
