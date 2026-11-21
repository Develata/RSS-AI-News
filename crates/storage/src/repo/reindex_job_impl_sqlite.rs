//! [`super::reindex_job::ReindexJobRepository`] 的 SQLite 方言实装。
//!
//! W11-P3-C-2 / 实装层超 800 行后按方言再拆：本文件持 `sqlite_*` free fn，
//! 由 [`super::reindex_job_impl`] 的 trait 分发 `match &self.pool` 调用；
//! SQL const 集中在 [`super::reindex_job_sql`]。

use sqlx::SqlitePool;
use time::OffsetDateTime;

use crate::{StorageError, classify_db_error};

use super::{sqlite_instant_cutoff, sqlite_stored_deadline};

use super::article::{
    SELECT_ARTICLE_CONTENT_HASH_COLLISION_SQL, SELECT_ARTICLE_CONTENT_HASH_SQL,
    UPDATE_ARTICLE_CONTENT_HASH_SQL,
};
use super::feed_entry_impl::reconcile_link_hash_move_sqlite;
use super::feed_entry_sql::SELECT_LINK_HASH_BY_ID_SQL;
use super::reindex_job::{
    ClaimedReindexJob, FinishReindexTxOutcome, ReindexBatchCheckpoint, ReindexJob,
    ResumeReindexOutcome, StartReindexTxOutcome,
};
use super::reindex_job_sql::{
    ABORT_SQL, ADVANCE_CHECKPOINT_SQL, ADVANCE_TO_COMPLETED_SQL, ASSERT_LEASE_HELD_SQL,
    CLAIM_BY_ID_SQLITE_SQL, CLAIM_PENDING_SQLITE_SQL, CLAIM_RESUMABLE_SQLITE_SQL,
    COMPLETE_WITHOUT_CLAIM_SQL, FIND_ACTIVE_JOB_RULE_SQLITE_SQL,
    FIND_ACTIVE_REINDEX_JOB_BY_TARGET_SQL, FIND_REINDEX_JOB_BY_ID_SQL,
    FINISH_REINDEX_DEMOTE_ACTIVE_SQL, FINISH_REINDEX_PROMOTE_PENDING_SQL,
    FINISH_REINDEX_UPDATE_JOB_SQL, INSERT_REINDEX_JOB_PENDING_SQL, INSERT_RULE_VERSION_PENDING_SQL,
    LIST_RUNNING_REINDEX_JOBS_SQL, LOCK_REINDEX_TARGET_SQLITE_SQL, MARK_FAILED_SQL,
    RECLAIM_EXPIRED_LEASES_SQLITE_SQL,
};
use crate::{UpdateContentHashOutcome, UpdateLinkHashOutcome};

pub(super) async fn sqlite_start_reindex_tx(
    pool: &SqlitePool,
    rule_kind: &str,
    rule_version_tag: &str,
    rule_description: &str,
    rule_payload_sha256: &str,
    target: &str,
    now: OffsetDateTime,
) -> Result<StartReindexTxOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;

    let rule_version_id = sqlx::query_scalar::<_, i64>(INSERT_RULE_VERSION_PENDING_SQL)
        .bind(rule_kind)
        .bind(rule_version_tag)
        .bind(rule_description)
        .bind(rule_payload_sha256)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            classify_db_error(
                error,
                "rule_versions",
                format!("{rule_kind}/{rule_version_tag}"),
            )
        })?;

    let job_id = sqlx::query_scalar::<_, i64>(INSERT_REINDEX_JOB_PENDING_SQL)
        .bind(target)
        .bind(rule_version_id)
        .bind(now)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            classify_db_error(
                error,
                "reindex_jobs",
                format!("target={target}/rule_version_id={rule_version_id}"),
            )
        })?;

    tx.commit().await.map_err(StorageError::from)?;
    Ok(StartReindexTxOutcome {
        rule_version_id,
        job_id,
    })
}

pub(super) async fn sqlite_complete_without_claim(
    pool: &SqlitePool,
    id: i64,
    finished_at: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(COMPLETE_WITHOUT_CLAIM_SQL)
        .bind(finished_at)
        .bind(finished_at)
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn sqlite_finish_reindex_tx(
    pool: &SqlitePool,
    job_id: i64,
    owner: &str,
    rule_version_id: i64,
    rule_kind: &str,
    finished_at: OffsetDateTime,
) -> Result<FinishReindexTxOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;

    let job_update = sqlx::query(FINISH_REINDEX_UPDATE_JOB_SQL)
        .bind(finished_at)
        .bind(finished_at)
        .bind(job_id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    if job_update.rows_affected() == 0 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(FinishReindexTxOutcome {
            job_completed: false,
            demoted_rule_version_id: None,
        });
    }

    let demoted_rule_version_id = sqlx::query_scalar::<_, i64>(FINISH_REINDEX_DEMOTE_ACTIVE_SQL)
        .bind(finished_at)
        .bind(rule_kind)
        .bind(rule_version_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;

    let promote = sqlx::query(FINISH_REINDEX_PROMOTE_PENDING_SQL)
        .bind(rule_version_id)
        .bind(rule_kind)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    if promote.rows_affected() != 1 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Err(StorageError::Conflict {
            table: "rule_versions".to_string(),
            key: format!("id={rule_version_id}/kind={rule_kind} 非 pending 状态"),
        });
    }

    tx.commit().await.map_err(StorageError::from)?;
    Ok(FinishReindexTxOutcome {
        job_completed: true,
        demoted_rule_version_id,
    })
}

pub(super) async fn sqlite_insert_pending(
    pool: &SqlitePool,
    target: &str,
    rule_version_id: i64,
    now: OffsetDateTime,
) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_REINDEX_JOB_PENDING_SQL)
        .bind(target)
        .bind(rule_version_id)
        .bind(now)
        .bind(now)
        .fetch_one(pool)
        .await
        .map_err(|error| {
            classify_db_error(
                error,
                "reindex_jobs",
                format!("target={target}/rule_version_id={rule_version_id}"),
            )
        })
}

pub(super) async fn sqlite_claim_pending(
    pool: &SqlitePool,
    owner: &str,
    now: OffsetDateTime,
    lease_expires_at: OffsetDateTime,
) -> Result<Option<ClaimedReindexJob>, StorageError> {
    sqlx::query_as::<_, ClaimedReindexJob>(CLAIM_PENDING_SQLITE_SQL)
        .bind(owner)
        .bind(sqlite_stored_deadline(lease_expires_at))
        .bind(now)
        .bind(now)
        .bind(sqlite_instant_cutoff(now))
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn sqlite_claim_by_id(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    now: OffsetDateTime,
    lease_expires_at: OffsetDateTime,
) -> Result<Option<ClaimedReindexJob>, StorageError> {
    sqlx::query_as::<_, ClaimedReindexJob>(CLAIM_BY_ID_SQLITE_SQL)
        .bind(owner)
        .bind(sqlite_stored_deadline(lease_expires_at))
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(sqlite_instant_cutoff(now))
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn sqlite_advance_checkpoint(
    pool: &SqlitePool,
    checkpoint: ReindexBatchCheckpoint<'_>,
) -> Result<bool, StorageError> {
    let mut conn = pool.acquire().await.map_err(StorageError::from)?;
    sqlite_checkpoint_on(&mut conn, checkpoint).await
}

/// [`ADVANCE_CHECKPOINT_SQL`] on any connection or open transaction.
async fn sqlite_checkpoint_on(
    conn: &mut sqlx::SqliteConnection,
    checkpoint: ReindexBatchCheckpoint<'_>,
) -> Result<bool, StorageError> {
    let result = sqlx::query(ADVANCE_CHECKPOINT_SQL)
        .bind(checkpoint.last_processed_id)
        .bind(checkpoint.now)
        .bind(checkpoint.job_id)
        .bind(checkpoint.owner)
        .bind(sqlite_stored_deadline(checkpoint.lease_expires_at))
        .execute(conn)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn sqlite_assert_lease_held(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(ASSERT_LEASE_HELD_SQL)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn sqlite_advance_to_completed(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    finished_at: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(ADVANCE_TO_COMPLETED_SQL)
        .bind(finished_at)
        .bind(finished_at)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn sqlite_mark_failed(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    error: &str,
    finished_at: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(MARK_FAILED_SQL)
        .bind(error)
        .bind(finished_at)
        .bind(finished_at)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn sqlite_abort(
    pool: &SqlitePool,
    id: i64,
    aborted_reason: &str,
    finished_at: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(ABORT_SQL)
        .bind(aborted_reason)
        .bind(finished_at)
        .bind(finished_at)
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn sqlite_reclaim_expired_leases(
    pool: &SqlitePool,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(RECLAIM_EXPIRED_LEASES_SQLITE_SQL)
        .bind(now)
        .bind(sqlite_instant_cutoff(now))
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

pub(super) async fn sqlite_list_running(
    pool: &SqlitePool,
) -> Result<Vec<ReindexJob>, StorageError> {
    sqlx::query_as::<_, ReindexJob>(LIST_RUNNING_REINDEX_JOBS_SQL)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn sqlite_find_by_id(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<ReindexJob>, StorageError> {
    sqlx::query_as::<_, ReindexJob>(FIND_REINDEX_JOB_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn sqlite_find_active_by_target(
    pool: &SqlitePool,
    target: &str,
) -> Result<Option<ReindexJob>, StorageError> {
    sqlx::query_as::<_, ReindexJob>(FIND_ACTIVE_REINDEX_JOB_BY_TARGET_SQL)
        .bind(target)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

/// `(job id, state, rule kind, version_tag, payload_sha256)`.
type ActiveJobRule = (i64, String, String, String, String);

pub(super) async fn sqlite_claim_resumable_by_target(
    pool: &SqlitePool,
    target: &str,
    rule_kind: &str,
    rule_payload_sha256: &str,
    owner: &str,
    now: OffsetDateTime,
    lease_expires_at: OffsetDateTime,
) -> Result<ResumeReindexOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    // Write lock first: lookup and claim are one serialised step.
    sqlx::query(LOCK_REINDEX_TARGET_SQLITE_SQL)
        .bind(target)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    let active = sqlx::query_as::<_, ActiveJobRule>(FIND_ACTIVE_JOB_RULE_SQLITE_SQL)
        .bind(target)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    let outcome = match active {
        None => ResumeReindexOutcome::NoActiveJob,
        Some((job_id, state, kind, version_tag, payload_sha256))
            if kind != rule_kind || payload_sha256 != rule_payload_sha256 =>
        {
            ResumeReindexOutcome::RuleMismatch {
                job_id,
                state,
                version_tag,
                payload_sha256,
            }
        }
        Some((job_id, state, ..)) => {
            let claimed = sqlx::query_as::<_, ClaimedReindexJob>(CLAIM_RESUMABLE_SQLITE_SQL)
                .bind(owner)
                .bind(sqlite_stored_deadline(lease_expires_at))
                .bind(now)
                .bind(job_id)
                .bind(sqlite_instant_cutoff(now))
                .fetch_optional(&mut *tx)
                .await
                .map_err(StorageError::from)?;
            match claimed {
                Some(claimed) => ResumeReindexOutcome::Resumed(claimed),
                None => ResumeReindexOutcome::Busy { job_id, state },
            }
        }
    };
    tx.commit().await.map_err(StorageError::from)?;
    Ok(outcome)
}

pub(super) async fn sqlite_apply_link_hash_batch(
    pool: &SqlitePool,
    checkpoint: ReindexBatchCheckpoint<'_>,
    updates: &[(i64, String)],
) -> Result<Option<Vec<UpdateLinkHashOutcome>>, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    if !sqlite_checkpoint_on(&mut tx, checkpoint).await? {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(None);
    }
    let mut outcomes = Vec::with_capacity(updates.len());
    for (id, new_link_hash) in updates {
        let old_link_hash = sqlx::query_scalar::<_, String>(SELECT_LINK_HASH_BY_ID_SQL)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        let Some(old_link_hash) = old_link_hash else {
            outcomes.push(UpdateLinkHashOutcome::Missing);
            continue;
        };
        outcomes.push(
            reconcile_link_hash_move_sqlite(
                &mut tx,
                *id,
                &old_link_hash,
                new_link_hash,
                checkpoint.now,
            )
            .await?,
        );
    }
    tx.commit().await.map_err(StorageError::from)?;
    Ok(Some(outcomes))
}

pub(super) async fn sqlite_apply_content_hash_batch(
    pool: &SqlitePool,
    checkpoint: ReindexBatchCheckpoint<'_>,
    updates: &[(i64, String)],
) -> Result<Option<Vec<UpdateContentHashOutcome>>, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    if !sqlite_checkpoint_on(&mut tx, checkpoint).await? {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(None);
    }
    let mut outcomes = Vec::with_capacity(updates.len());
    for (id, new_content_hash) in updates {
        // Pre-checks instead of catching the unique violation: a failed
        // statement would abort the whole PG transaction.
        let current = sqlx::query_scalar::<_, String>(SELECT_ARTICLE_CONTENT_HASH_SQL)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        let outcome = match current {
            None => UpdateContentHashOutcome::Conflict,
            Some(current) if current == *new_content_hash => UpdateContentHashOutcome::Unchanged,
            Some(_) => {
                let taken = sqlx::query_scalar::<_, i32>(SELECT_ARTICLE_CONTENT_HASH_COLLISION_SQL)
                    .bind(new_content_hash)
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(StorageError::from)?
                    != 0;
                if taken {
                    UpdateContentHashOutcome::Conflict
                } else {
                    sqlx::query(UPDATE_ARTICLE_CONTENT_HASH_SQL)
                        .bind(new_content_hash)
                        .bind(checkpoint.now)
                        .bind(id)
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::from)?;
                    UpdateContentHashOutcome::Updated
                }
            }
        };
        outcomes.push(outcome);
    }
    tx.commit().await.map_err(StorageError::from)?;
    Ok(Some(outcomes))
}
