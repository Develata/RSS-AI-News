//! [`PublishRecordRepository`] trait 实装。
//!
//! W11-P3-C-4：所有方法按 backend `match` 分发到 sqlite_*/pg_* 私有 helper；
//! SQL 跨方言完全等价（const 集中在 [`super::publish_record_sql`]）；claim
//! 路径在 PG 加 `FOR UPDATE SKIP LOCKED`（§6.4 契约）。

use async_trait::async_trait;
use sqlx::{PgPool, Postgres, QueryBuilder, Sqlite, SqlitePool};
use time::OffsetDateTime;

use rss_ai_news_domain::dto::publish::RemoteBlob;

use crate::{ClaimRequest, ReleaseFailureOutcome, StorageError, StoragePool, classify_db_error};

use super::{
    publish_record::{
        ClaimedPublishRecord, DestinationCheck, NewPublishRecord, PublishAdvanceExtras,
        PublishRecord, PublishRecordRepo, PublishRecordRepository, PublishState,
        PublishTimestampField, RecordedWrite, TerminalAdvanceOutcome, TerminalAdvanceStatus,
        UnverifiedWriteUpdate,
    },
    publish_record_sql::{
        ADVANCE_LOCAL_SQL, ADVANCE_REMOTE_SQL, ADVANCE_RENDERED_SQL, ADVANCE_SNAPSHOT_SQL,
        CREATE_IF_NEW_SQL, FIND_REMOTE_DESTINATION_HOLDER_PG_SQL,
        FIND_REMOTE_DESTINATION_HOLDER_SQLITE_SQL, LOCK_REMOTE_DESTINATION_PG_SQL,
        MARK_REMOTE_WRITE_PENDING_SQL, OWNS_PUBLISH_RECORD_SQL, PROMOTE_ARTICLE_PUBLISHED_SQL,
        PROMOTE_ARTICLES_PUBLISHED_BATCH_PG_SQL, RECLAIM_PUBLISH_LEASES_SQL,
        RECLAIM_PUBLISH_LEASES_SQLITE_SQL, RELEASE_DEFERRED_RETRY_RECORDING_SQL,
        RELEASE_DEFERRED_RETRY_SQL, RELEASE_EMPTY_SNAPSHOT_SQL, RELEASE_FREEZE_CONFLICT_SQL,
        RELEASE_OWNERSHIP_CONFLICT_SQL, RELEASE_PERMANENT_FAILURE_SQL,
        RELEASE_PUBLISH_RETRYABLE_FAILURE_KEEPING_WRITE_SQL, RELEASE_PUBLISH_RETRYABLE_FAILURE_SQL,
        SELECT_PUBLISH_RECORD_BY_ID, SELECT_PUBLISH_RECORD_BY_IDEMPOTENCY_KEY,
        TERMINALIZE_EXHAUSTED_PUBLISH_SQL, TERMINALIZE_EXHAUSTED_PUBLISH_SQLITE_SQL,
        claim_pending_in_category_pg, claim_pending_in_category_sqlite, claim_publish_pg,
        claim_publish_sqlite, sqlite_lease_cutoff, sqlite_stored_deadline,
    },
};

fn advance_sql(field: PublishTimestampField) -> &'static str {
    match field {
        PublishTimestampField::SnapshotFrozenAt => ADVANCE_SNAPSHOT_SQL,
        PublishTimestampField::RenderedAt => ADVANCE_RENDERED_SQL,
        PublishTimestampField::LocalStoredAt => ADVANCE_LOCAL_SQL,
        PublishTimestampField::RemotePublishedAt => ADVANCE_REMOTE_SQL,
    }
}

#[async_trait]
impl PublishRecordRepository for PublishRecordRepo {
    async fn create_if_new(&self, item: &NewPublishRecord) -> Result<Option<i64>, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_create_if_new(p, item).await,
            StoragePool::Postgres(p) => pg_create_if_new(p, item).await,
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<PublishRecord>, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_find_by_id(p, id).await,
            StoragePool::Postgres(p) => pg_find_by_id(p, id).await,
        }
    }

    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<PublishRecord>, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_find_by_idempotency_key(p, key).await,
            StoragePool::Postgres(p) => pg_find_by_idempotency_key(p, key).await,
        }
    }

    async fn claim_pending_for_freeze(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => claim_publish_sqlite(p, request, "pending").await,
            StoragePool::Postgres(p) => claim_publish_pg(p, request, "pending").await,
        }
    }

    async fn claim_pending_for_freeze_in_category(
        &self,
        request: &ClaimRequest,
        category_key: &str,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => {
                claim_pending_in_category_sqlite(p, request, category_key).await
            }
            StoragePool::Postgres(p) => {
                claim_pending_in_category_pg(p, request, category_key).await
            }
        }
    }

    async fn claim_frozen_for_render(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => claim_publish_sqlite(p, request, "snapshot_frozen").await,
            StoragePool::Postgres(p) => claim_publish_pg(p, request, "snapshot_frozen").await,
        }
    }

    async fn claim_rendered_for_local_store(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => claim_publish_sqlite(p, request, "rendered").await,
            StoragePool::Postgres(p) => claim_publish_pg(p, request, "rendered").await,
        }
    }

    async fn claim_local_for_remote_publish(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => claim_publish_sqlite(p, request, "stored_local").await,
            StoragePool::Postgres(p) => claim_publish_pg(p, request, "stored_local").await,
        }
    }

    async fn claim_local_for_remote_publish_by_ids(
        &self,
        request: &ClaimRequest,
        ids: &[i64],
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.claim_publish_by_ids(request, PublishState::StoredLocal, ids)
            .await
    }

    async fn claim_publish_by_ids(
        &self,
        request: &ClaimRequest,
        state: PublishState,
        ids: &[i64],
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        match self.storage_pool() {
            StoragePool::Sqlite(p) => {
                sqlite_claim_by_ids(p, request, state.as_str(), ids, None).await
            }
            StoragePool::Postgres(p) => {
                pg_claim_by_ids(p, request, state.as_str(), ids, None).await
            }
        }
    }

    async fn claim_pending_for_freeze_by_id_in_category(
        &self,
        request: &ClaimRequest,
        id: i64,
        category_key: &str,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        let state = PublishState::Pending.as_str();
        let category = Some(category_key);
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_claim_by_ids(p, request, state, &[id], category).await,
            StoragePool::Postgres(p) => pg_claim_by_ids(p, request, state, &[id], category).await,
        }
    }

    async fn release_advance(
        &self,
        id: i64,
        owner: &str,
        from: PublishState,
        to: PublishState,
        timestamp_field: PublishTimestampField,
        now: OffsetDateTime,
        extras: PublishAdvanceExtras,
    ) -> Result<bool, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => {
                sqlite_release_advance(p, id, owner, from, to, timestamp_field, now, extras).await
            }
            StoragePool::Postgres(p) => {
                pg_release_advance(p, id, owner, from, to, timestamp_field, now, extras).await
            }
        }
    }

    async fn release_retryable_failure_until(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        retry_not_before: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError> {
        let release = RetryableRelease {
            id,
            owner,
            error,
            kind,
            max_attempts,
            retry_not_before,
            now,
        };
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_release_retryable_failure(p, release).await,
            StoragePool::Postgres(p) => pg_release_retryable_failure(p, release).await,
        }
    }

    async fn release_empty_snapshot(
        &self,
        id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let affected = match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlx::query(RELEASE_EMPTY_SNAPSHOT_SQL)
                .bind(now)
                .bind(id)
                .bind(owner)
                .execute(p)
                .await
                .map(|result| result.rows_affected()),
            StoragePool::Postgres(p) => sqlx::query(RELEASE_EMPTY_SNAPSHOT_SQL)
                .bind(now)
                .bind(id)
                .bind(owner)
                .execute(p)
                .await
                .map(|result| result.rows_affected()),
        }
        .map_err(StorageError::from)?;
        Ok(affected == 1)
    }

    async fn release_freeze_conflict(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let affected = match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlx::query(RELEASE_FREEZE_CONFLICT_SQL)
                .bind(error)
                .bind(kind)
                .bind(now)
                .bind(id)
                .bind(owner)
                .execute(p)
                .await
                .map_err(StorageError::from)?
                .rows_affected(),
            StoragePool::Postgres(p) => sqlx::query(RELEASE_FREEZE_CONFLICT_SQL)
                .bind(error)
                .bind(kind)
                .bind(now)
                .bind(id)
                .bind(owner)
                .execute(p)
                .await
                .map_err(StorageError::from)?
                .rows_affected(),
        };
        Ok(affected == 1)
    }

    async fn release_ownership_conflict(
        &self,
        id: i64,
        owner: &str,
        state: PublishState,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let affected = match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlx::query(RELEASE_OWNERSHIP_CONFLICT_SQL)
                .bind(error)
                .bind(kind)
                .bind(now)
                .bind(id)
                .bind(owner)
                .bind(state.as_str())
                .execute(p)
                .await
                .map_err(StorageError::from)?
                .rows_affected(),
            StoragePool::Postgres(p) => sqlx::query(RELEASE_OWNERSHIP_CONFLICT_SQL)
                .bind(error)
                .bind(kind)
                .bind(now)
                .bind(id)
                .bind(owner)
                .bind(state.as_str())
                .execute(p)
                .await
                .map_err(StorageError::from)?
                .rows_affected(),
        };
        Ok(affected == 1)
    }

    async fn release_deferred_retry(
        &self,
        id: i64,
        owner: &str,
        state: PublishState,
        error: &str,
        kind: &str,
        retry_not_before: Option<OffsetDateTime>,
        unverified_write: UnverifiedWriteUpdate<'_>,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        // `Keep` leaves the recorded write untouched; `Record` binds $8 / $9.
        let (sql, recorded) = match unverified_write {
            UnverifiedWriteUpdate::Keep => (RELEASE_DEFERRED_RETRY_SQL, None),
            UnverifiedWriteUpdate::Record(blob) => (
                RELEASE_DEFERRED_RETRY_RECORDING_SQL,
                Some((
                    blob.map(|blob| blob.path.as_str()),
                    blob.map(|blob| blob.blob_sha.as_str()),
                )),
            ),
        };
        let affected = match self.storage_pool() {
            StoragePool::Sqlite(p) => {
                let mut query = sqlx::query(sql)
                    .bind(error)
                    .bind(kind)
                    .bind(now)
                    .bind(id)
                    .bind(owner)
                    .bind(state.as_str())
                    // Rounded up like every SQLite deadline: never claimable early.
                    .bind(retry_not_before.map(sqlite_stored_deadline));
                if let Some((path, blob_sha)) = recorded {
                    query = query.bind(path).bind(blob_sha);
                }
                query
                    .execute(p)
                    .await
                    .map_err(StorageError::from)?
                    .rows_affected()
            }
            StoragePool::Postgres(p) => {
                let mut query = sqlx::query(sql)
                    .bind(error)
                    .bind(kind)
                    .bind(now)
                    .bind(id)
                    .bind(owner)
                    .bind(state.as_str())
                    .bind(retry_not_before);
                if let Some((path, blob_sha)) = recorded {
                    query = query.bind(path).bind(blob_sha);
                }
                query
                    .execute(p)
                    .await
                    .map_err(StorageError::from)?
                    .rows_affected()
            }
        };
        Ok(affected == 1)
    }

    async fn record_remote_write(
        &self,
        id: i64,
        owner: &str,
        state: PublishState,
        error: &str,
        kind: &str,
        blob: Option<&RemoteBlob>,
        check: Option<DestinationCheck<'_>>,
        now: OffsetDateTime,
    ) -> Result<RecordedWrite, StorageError> {
        let write = PendingWrite {
            id,
            owner,
            state,
            error,
            kind,
            blob,
            now,
        };
        // The holder check needs the destination path, i.e. a recorded blob.
        let check = check.zip(blob).map(|(check, blob)| FoldedCheck {
            category_key: check.category_key,
            path: blob.path.to_ascii_lowercase(),
            target_identity: check.target_identity.to_ascii_lowercase(),
            receipt_target: check.receipt_target.to_ascii_lowercase(),
        });
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_record_remote_write(p, &write, check.as_ref()).await,
            StoragePool::Postgres(p) => pg_record_remote_write(p, &write, check.as_ref()).await,
        }
    }

    async fn release_retryable_failure_keeping_write(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        retry_not_before: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError> {
        let release = RetryableRelease {
            id,
            owner,
            error,
            kind,
            max_attempts,
            retry_not_before,
            now,
        };
        let sql = RELEASE_PUBLISH_RETRYABLE_FAILURE_KEEPING_WRITE_SQL;
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_release_retryable_failure_with(p, sql, release).await,
            StoragePool::Postgres(p) => pg_release_retryable_failure_with(p, sql, release).await,
        }
    }

    async fn owns_publish_records(
        &self,
        ids: &[i64],
        owner: &str,
        state: PublishState,
    ) -> Result<bool, StorageError> {
        for &id in ids {
            let held = match self.storage_pool() {
                StoragePool::Sqlite(p) => sqlx::query_scalar::<_, i64>(OWNS_PUBLISH_RECORD_SQL)
                    .bind(id)
                    .bind(owner)
                    .bind(state.as_str())
                    .fetch_optional(p)
                    .await
                    .map_err(StorageError::from)?,
                StoragePool::Postgres(p) => sqlx::query_scalar::<_, i64>(OWNS_PUBLISH_RECORD_SQL)
                    .bind(id)
                    .bind(owner)
                    .bind(state.as_str())
                    .fetch_optional(p)
                    .await
                    .map_err(StorageError::from)?,
            };
            if held.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => {
                sqlite_release_permanent_failure(p, id, owner, error, kind, now).await
            }
            StoragePool::Postgres(p) => {
                pg_release_permanent_failure(p, id, owner, error, kind, now).await
            }
        }
    }

    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_reclaim_expired_leases(p, now).await,
            StoragePool::Postgres(p) => pg_reclaim_expired_leases(p, now).await,
        }
    }

    async fn terminalize_exhausted(
        &self,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => sqlite_terminalize_exhausted(p, max_attempts, now).await,
            StoragePool::Postgres(p) => pg_terminalize_exhausted(p, max_attempts, now).await,
        }
    }

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
    ) -> Result<TerminalAdvanceOutcome, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => {
                sqlite_release_terminal_advance(
                    p,
                    id,
                    owner,
                    from,
                    to,
                    timestamp_field,
                    promote_article_ids,
                    extras,
                    now,
                )
                .await
            }
            StoragePool::Postgres(p) => {
                pg_release_terminal_advance(
                    p,
                    id,
                    owner,
                    from,
                    to,
                    timestamp_field,
                    promote_article_ids,
                    extras,
                    now,
                )
                .await
            }
        }
    }
}

// ── SQLite helper ──────────────────────────────────────────────

async fn sqlite_create_if_new(
    pool: &SqlitePool,
    item: &NewPublishRecord,
) -> Result<Option<i64>, StorageError> {
    sqlx::query_scalar::<_, i64>(CREATE_IF_NEW_SQL)
        .bind(&item.idempotency_key)
        .bind(&item.category_key)
        .bind(&item.report_date)
        .bind(&item.target_timezone)
        .bind(item.render_version)
        .bind(item.selection_policy_version)
        .bind(&item.remote_target)
        .fetch_optional(pool)
        .await
        .map_err(|error| classify_db_error(error, "publish_records", &item.idempotency_key))
}

async fn sqlite_find_by_id(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<PublishRecord>, StorageError> {
    sqlx::query_as::<_, PublishRecord>(SELECT_PUBLISH_RECORD_BY_ID)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

async fn sqlite_find_by_idempotency_key(
    pool: &SqlitePool,
    key: &str,
) -> Result<Option<PublishRecord>, StorageError> {
    sqlx::query_as::<_, PublishRecord>(SELECT_PUBLISH_RECORD_BY_IDEMPOTENCY_KEY)
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

#[allow(clippy::too_many_arguments)]
async fn sqlite_release_advance(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    from: PublishState,
    to: PublishState,
    timestamp_field: PublishTimestampField,
    now: OffsetDateTime,
    extras: PublishAdvanceExtras,
) -> Result<bool, StorageError> {
    let result = sqlx::query(advance_sql(timestamp_field))
        .bind(to.as_str())
        .bind(now)
        .bind(&extras.local_path)
        .bind(&extras.remote_target)
        .bind(&extras.commit_sha)
        .bind(now)
        .bind(id)
        .bind(owner)
        .bind(from.as_str())
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

/// Arguments of a retryable release (both dialects bind them in SQL order).
struct RetryableRelease<'a> {
    id: i64,
    owner: &'a str,
    error: &'a str,
    kind: &'a str,
    max_attempts: u32,
    retry_not_before: Option<OffsetDateTime>,
    now: OffsetDateTime,
}

/// Arguments of [`PublishRecordRepository::record_remote_write`].
struct PendingWrite<'a> {
    id: i64,
    owner: &'a str,
    state: PublishState,
    error: &'a str,
    kind: &'a str,
    blob: Option<&'a RemoteBlob>,
    now: OffsetDateTime,
}

/// The R5-5 destination check with its comparands folded to ASCII lower
/// case (R5-8); `receipt_target` is also the PostgreSQL lock key (R5-7).
struct FoldedCheck<'a> {
    category_key: &'a str,
    path: String,
    target_identity: String,
    receipt_target: String,
}

/// SQLite: the guarded UPDATE comes first and takes the database write lock
/// (the freeze convention, ADR 0012 §2), so a concurrent record-and-check of
/// the same destination waits for this transaction and then sees its record.
async fn sqlite_record_remote_write(
    pool: &SqlitePool,
    write: &PendingWrite<'_>,
    check: Option<&FoldedCheck<'_>>,
) -> Result<RecordedWrite, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let recorded = sqlx::query(MARK_REMOTE_WRITE_PENDING_SQL)
        .bind(write.error)
        .bind(write.kind)
        .bind(write.blob.map(|blob| blob.path.as_str()))
        .bind(write.blob.map(|blob| blob.blob_sha.as_str()))
        .bind(write.now)
        .bind(write.id)
        .bind(write.owner)
        .bind(write.state.as_str())
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?
        .rows_affected();
    if recorded != 1 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(RecordedWrite::LeaseLost);
    }
    if let Some(check) = check {
        let holder = sqlx::query_scalar::<_, i64>(FIND_REMOTE_DESTINATION_HOLDER_SQLITE_SQL)
            .bind(write.id)
            .bind(check.category_key)
            .bind(&check.path)
            .bind(write.kind)
            .bind(&check.target_identity)
            .bind(&check.receipt_target)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        if let Some(holder) = holder {
            tx.rollback().await.map_err(StorageError::from)?;
            return Ok(RecordedWrite::HeldBy(holder));
        }
    }
    tx.commit().await.map_err(StorageError::from)?;
    Ok(RecordedWrite::Recorded)
}

/// PostgreSQL: a row lock on this record serialises nothing, so the
/// destination itself is locked (transaction-scoped advisory lock on its
/// folded receipt). READ COMMITTED, set explicitly as in freeze, makes the
/// holder query after the lock see whatever the previous holder committed.
async fn pg_record_remote_write(
    pool: &PgPool,
    write: &PendingWrite<'_>,
    check: Option<&FoldedCheck<'_>>,
) -> Result<RecordedWrite, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    if let Some(check) = check {
        sqlx::query(LOCK_REMOTE_DESTINATION_PG_SQL)
            .bind(&check.receipt_target)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::from)?;
    }
    let recorded = sqlx::query(MARK_REMOTE_WRITE_PENDING_SQL)
        .bind(write.error)
        .bind(write.kind)
        .bind(write.blob.map(|blob| blob.path.as_str()))
        .bind(write.blob.map(|blob| blob.blob_sha.as_str()))
        .bind(write.now)
        .bind(write.id)
        .bind(write.owner)
        .bind(write.state.as_str())
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?
        .rows_affected();
    if recorded != 1 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(RecordedWrite::LeaseLost);
    }
    if let Some(check) = check {
        let holder = sqlx::query_scalar::<_, i64>(FIND_REMOTE_DESTINATION_HOLDER_PG_SQL)
            .bind(write.id)
            .bind(check.category_key)
            .bind(&check.path)
            .bind(write.kind)
            .bind(&check.target_identity)
            .bind(&check.receipt_target)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        if let Some(holder) = holder {
            tx.rollback().await.map_err(StorageError::from)?;
            return Ok(RecordedWrite::HeldBy(holder));
        }
    }
    tx.commit().await.map_err(StorageError::from)?;
    Ok(RecordedWrite::Recorded)
}

async fn sqlite_release_retryable_failure(
    pool: &SqlitePool,
    release: RetryableRelease<'_>,
) -> Result<ReleaseFailureOutcome, StorageError> {
    sqlite_release_retryable_failure_with(pool, RELEASE_PUBLISH_RETRYABLE_FAILURE_SQL, release)
        .await
}

/// The retryable release with `sql` (clearing or keeping the recorded write).
async fn sqlite_release_retryable_failure_with(
    pool: &SqlitePool,
    sql: &'static str,
    release: RetryableRelease<'_>,
) -> Result<ReleaseFailureOutcome, StorageError> {
    let state = sqlx::query_scalar::<_, String>(sql)
        .bind(i64::from(release.max_attempts))
        .bind(release.error)
        .bind(release.kind)
        .bind(release.now)
        .bind(release.id)
        .bind(release.owner)
        // Written rounded up to a whole millisecond and compared by
        // julianday against a rounded-down now: never claimable early.
        .bind(release.retry_not_before.map(sqlite_stored_deadline))
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(ReleaseFailureOutcome {
        released: state.is_some(),
        exhausted: state.as_deref() == Some("failed"),
    })
}

async fn sqlite_release_permanent_failure(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_PERMANENT_FAILURE_SQL)
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
    let result = sqlx::query(RECLAIM_PUBLISH_LEASES_SQLITE_SQL)
        .bind(now)
        .bind(sqlite_lease_cutoff(now))
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

async fn sqlite_terminalize_exhausted(
    pool: &SqlitePool,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(TERMINALIZE_EXHAUSTED_PUBLISH_SQLITE_SQL)
        .bind(now)
        .bind(i64::from(max_attempts))
        .bind(sqlite_lease_cutoff(now))
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

/// `category_key`: `Some` additionally requires the record's category
/// (R3F-S15).
async fn sqlite_claim_by_ids(
    pool: &SqlitePool,
    request: &ClaimRequest,
    state: &str,
    ids: &[i64],
    category_key: Option<&str>,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    let mut builder = QueryBuilder::<Sqlite>::new("UPDATE publish_records SET lease_owner = ");
    let cutoff = sqlite_lease_cutoff(request.now);
    builder
        .push_bind(&request.owner)
        .push(", lease_expires_at = ")
        .push_bind(sqlite_stored_deadline(request.lease_expires_at))
        .push(", attempt_count = attempt_count + 1, stage_attempt_count = stage_attempt_count + 1, updated_at = ")
        .push_bind(request.now)
        .push(" WHERE state = ")
        .push_bind(state)
        .push(" AND id IN (");
    {
        let mut separated = builder.separated(", ");
        for id in ids {
            separated.push_bind(id);
        }
    }
    // Instant-correct lease expiry (see publish_record_sql.rs).
    builder
        .push(
            ") AND (lease_expires_at IS NULL OR COALESCE(julianday(lease_expires_at) < julianday(",
        )
        .push_bind(cutoff)
        .push("), lease_expires_at < ")
        .push_bind(cutoff)
        // Rate-limit wait (R1-P-S12), instant-correct like the lease.
        .push(
            ")) AND (next_attempt_at IS NULL OR COALESCE(julianday(next_attempt_at) <= julianday(",
        )
        .push_bind(cutoff)
        .push("), next_attempt_at <= ")
        .push_bind(cutoff)
        .push(")) AND stage_attempt_count < ")
        .push_bind(i64::from(request.max_attempts));
    if let Some(category_key) = category_key {
        builder.push(" AND category_key = ").push_bind(category_key);
    }
    builder
        .push(" RETURNING id, idempotency_key, category_key, report_date, target_timezone, ")
        .push("render_version, selection_policy_version, state, remote_target, attempt_count, rendered_at, last_error_kind, unverified_write_path, unverified_write_blob_sha");

    builder
        .build_query_as::<ClaimedPublishRecord>()
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

#[allow(clippy::too_many_arguments)]
async fn sqlite_release_terminal_advance(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    from: PublishState,
    to: PublishState,
    timestamp_field: PublishTimestampField,
    promote_article_ids: Vec<i64>,
    extras: PublishAdvanceExtras,
    now: OffsetDateTime,
) -> Result<TerminalAdvanceOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let result = sqlx::query(advance_sql(timestamp_field))
        .bind(to.as_str())
        .bind(now)
        .bind(&extras.local_path)
        .bind(&extras.remote_target)
        .bind(&extras.commit_sha)
        .bind(now)
        .bind(id)
        .bind(owner)
        .bind(from.as_str())
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    if result.rows_affected() != 1 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(TerminalAdvanceOutcome {
            status: TerminalAdvanceStatus::PublishRecordConflict,
        });
    }

    for article_id in promote_article_ids {
        let result = sqlx::query(PROMOTE_ARTICLE_PUBLISHED_SQL)
            .bind(now)
            .bind(article_id)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        if result.rows_affected() != 1 {
            tx.rollback().await.map_err(StorageError::from)?;
            return Ok(TerminalAdvanceOutcome {
                status: TerminalAdvanceStatus::ArticleStateConflict { article_id },
            });
        }
    }

    tx.commit().await.map_err(StorageError::from)?;
    Ok(TerminalAdvanceOutcome {
        status: TerminalAdvanceStatus::Advanced,
    })
}

// ── PostgreSQL helper（W11-P3-C-4） ─────────────────────────────

async fn pg_create_if_new(
    pool: &PgPool,
    item: &NewPublishRecord,
) -> Result<Option<i64>, StorageError> {
    sqlx::query_scalar::<_, i64>(CREATE_IF_NEW_SQL)
        .bind(&item.idempotency_key)
        .bind(&item.category_key)
        .bind(&item.report_date)
        .bind(&item.target_timezone)
        .bind(item.render_version)
        .bind(item.selection_policy_version)
        .bind(&item.remote_target)
        .fetch_optional(pool)
        .await
        .map_err(|error| classify_db_error(error, "publish_records", &item.idempotency_key))
}

async fn pg_find_by_id(pool: &PgPool, id: i64) -> Result<Option<PublishRecord>, StorageError> {
    sqlx::query_as::<_, PublishRecord>(SELECT_PUBLISH_RECORD_BY_ID)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

async fn pg_find_by_idempotency_key(
    pool: &PgPool,
    key: &str,
) -> Result<Option<PublishRecord>, StorageError> {
    sqlx::query_as::<_, PublishRecord>(SELECT_PUBLISH_RECORD_BY_IDEMPOTENCY_KEY)
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

#[allow(clippy::too_many_arguments)]
async fn pg_release_advance(
    pool: &PgPool,
    id: i64,
    owner: &str,
    from: PublishState,
    to: PublishState,
    timestamp_field: PublishTimestampField,
    now: OffsetDateTime,
    extras: PublishAdvanceExtras,
) -> Result<bool, StorageError> {
    let result = sqlx::query(advance_sql(timestamp_field))
        .bind(to.as_str())
        .bind(now)
        .bind(&extras.local_path)
        .bind(&extras.remote_target)
        .bind(&extras.commit_sha)
        .bind(now)
        .bind(id)
        .bind(owner)
        .bind(from.as_str())
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn pg_release_retryable_failure(
    pool: &PgPool,
    release: RetryableRelease<'_>,
) -> Result<ReleaseFailureOutcome, StorageError> {
    pg_release_retryable_failure_with(pool, RELEASE_PUBLISH_RETRYABLE_FAILURE_SQL, release).await
}

/// The retryable release with `sql` (clearing or keeping the recorded write).
async fn pg_release_retryable_failure_with(
    pool: &PgPool,
    sql: &'static str,
    release: RetryableRelease<'_>,
) -> Result<ReleaseFailureOutcome, StorageError> {
    let state = sqlx::query_scalar::<_, String>(sql)
        .bind(i64::from(release.max_attempts))
        .bind(release.error)
        .bind(release.kind)
        .bind(release.now)
        .bind(release.id)
        .bind(release.owner)
        .bind(release.retry_not_before)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(ReleaseFailureOutcome {
        released: state.is_some(),
        exhausted: state.as_deref() == Some("failed"),
    })
}

async fn pg_release_permanent_failure(
    pool: &PgPool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_PERMANENT_FAILURE_SQL)
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
    let result = sqlx::query(RECLAIM_PUBLISH_LEASES_SQL)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

async fn pg_terminalize_exhausted(
    pool: &PgPool,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(TERMINALIZE_EXHAUSTED_PUBLISH_SQL)
        .bind(now)
        .bind(i64::from(max_attempts))
        .bind(now)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

/// PG claim by ids: same filters as [`sqlite_claim_by_ids`], but the candidate
/// rows are locked with `FOR UPDATE SKIP LOCKED` (like the global claim) so a
/// row held by another transaction is skipped instead of blocking this claim.
async fn pg_claim_by_ids(
    pool: &PgPool,
    request: &ClaimRequest,
    state: &str,
    ids: &[i64],
    category_key: Option<&str>,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    let mut builder = QueryBuilder::<Postgres>::new("UPDATE publish_records SET lease_owner = ");
    builder
        .push_bind(&request.owner)
        .push(", lease_expires_at = ")
        .push_bind(request.lease_expires_at)
        .push(", attempt_count = attempt_count + 1, stage_attempt_count = stage_attempt_count + 1, updated_at = ")
        .push_bind(request.now)
        .push(" WHERE id IN (SELECT id FROM publish_records WHERE state = ")
        .push_bind(state)
        .push(" AND id IN (");
    {
        let mut separated = builder.separated(", ");
        for id in ids {
            separated.push_bind(id);
        }
    }
    builder
        .push(") AND (lease_expires_at IS NULL OR lease_expires_at < ")
        .push_bind(request.now)
        .push(") AND (next_attempt_at IS NULL OR next_attempt_at <= ")
        .push_bind(request.now)
        .push(") AND stage_attempt_count < ")
        .push_bind(i64::from(request.max_attempts));
    if let Some(category_key) = category_key {
        builder.push(" AND category_key = ").push_bind(category_key);
    }
    builder
        .push(" ORDER BY id FOR UPDATE SKIP LOCKED)")
        .push(" RETURNING id, idempotency_key, category_key, report_date, target_timezone, ")
        .push("render_version, selection_policy_version, state, remote_target, attempt_count, rendered_at, last_error_kind, unverified_write_path, unverified_write_blob_sha");

    builder
        .build_query_as::<ClaimedPublishRecord>()
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

#[allow(clippy::too_many_arguments)]
async fn pg_release_terminal_advance(
    pool: &PgPool,
    id: i64,
    owner: &str,
    from: PublishState,
    to: PublishState,
    timestamp_field: PublishTimestampField,
    promote_article_ids: Vec<i64>,
    extras: PublishAdvanceExtras,
    now: OffsetDateTime,
) -> Result<TerminalAdvanceOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let result = sqlx::query(advance_sql(timestamp_field))
        .bind(to.as_str())
        .bind(now)
        .bind(&extras.local_path)
        .bind(&extras.remote_target)
        .bind(&extras.commit_sha)
        .bind(now)
        .bind(id)
        .bind(owner)
        .bind(from.as_str())
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    if result.rows_affected() != 1 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(TerminalAdvanceOutcome {
            status: TerminalAdvanceStatus::PublishRecordConflict,
        });
    }

    // codex P3-C 评审 LOW-1 修复：PG 批量 `id = ANY($2)` 替代 N+1 逐行 UPDATE，
    // 缩短大 batch 下事务时间与行锁持有窗口；RETURNING id 给冲突检测用。
    if !promote_article_ids.is_empty() {
        let promoted_ids: Vec<i64> = sqlx::query_scalar(PROMOTE_ARTICLES_PUBLISHED_BATCH_PG_SQL)
            .bind(now)
            .bind(&promote_article_ids)
            .fetch_all(&mut *tx)
            .await
            .map_err(StorageError::from)?;

        if promoted_ids.len() != promote_article_ids.len() {
            // 找出第一个未 promote 的 id（state != 'ready_for_publish' 或行已删）。
            // 保留 ArticleStateConflict 单 id 语义，与 SQLite 逐行实装一致。
            let promoted_set: std::collections::HashSet<i64> =
                promoted_ids.iter().copied().collect();
            let missing = promote_article_ids
                .iter()
                .copied()
                .find(|aid| !promoted_set.contains(aid));
            if let Some(article_id) = missing {
                tx.rollback().await.map_err(StorageError::from)?;
                return Ok(TerminalAdvanceOutcome {
                    status: TerminalAdvanceStatus::ArticleStateConflict { article_id },
                });
            }
        }
    }

    tx.commit().await.map_err(StorageError::from)?;
    Ok(TerminalAdvanceOutcome {
        status: TerminalAdvanceStatus::Advanced,
    })
}
