//! [`super::reindex_job::ReindexJobRepository`] 的 PostgreSQL 方言实装。
//!
//! W11-P3-C-2 / 实装层超 800 行后按方言再拆：本文件持 `pg_*` free fn，
//! 由 [`super::reindex_job_impl`] 的 trait 分发 `match &self.pool` 调用；
//! claim 路径用 `FOR UPDATE SKIP LOCKED`（§6.4）。SQL const 见
//! [`super::reindex_job_sql`]。

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{StorageError, classify_db_error};

use super::reindex_job::{
    ClaimedReindexJob, FinishReindexTxOutcome, ReindexJob, StartReindexTxOutcome,
};
use super::reindex_job_sql::{
    ABORT_SQL, ADVANCE_CHECKPOINT_SQL, ADVANCE_TO_COMPLETED_SQL, ASSERT_LEASE_HELD_SQL,
    CLAIM_BY_ID_PG_SQL, CLAIM_PENDING_PG_SQL, COMPLETE_WITHOUT_CLAIM_SQL,
    FINISH_REINDEX_DEMOTE_ACTIVE_SQL, FINISH_REINDEX_PROMOTE_PENDING_SQL,
    FINISH_REINDEX_UPDATE_JOB_SQL, INSERT_REINDEX_JOB_PENDING_SQL, INSERT_RULE_VERSION_PENDING_SQL,
    MARK_FAILED_SQL, RECLAIM_EXPIRED_LEASES_SQL, SELECT_REINDEX_JOB_COLUMNS,
};

pub(super) async fn pg_start_reindex_tx(
    pool: &PgPool,
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

pub(super) async fn pg_complete_without_claim(
    pool: &PgPool,
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

pub(super) async fn pg_finish_reindex_tx(
    pool: &PgPool,
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

pub(super) async fn pg_insert_pending(
    pool: &PgPool,
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

pub(super) async fn pg_claim_pending(
    pool: &PgPool,
    owner: &str,
    now: OffsetDateTime,
    lease_expires_at: OffsetDateTime,
) -> Result<Option<ClaimedReindexJob>, StorageError> {
    sqlx::query_as::<_, ClaimedReindexJob>(CLAIM_PENDING_PG_SQL)
        .bind(owner)
        .bind(lease_expires_at)
        .bind(now)
        .bind(now)
        .bind(now)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn pg_claim_by_id(
    pool: &PgPool,
    id: i64,
    owner: &str,
    now: OffsetDateTime,
    lease_expires_at: OffsetDateTime,
) -> Result<Option<ClaimedReindexJob>, StorageError> {
    sqlx::query_as::<_, ClaimedReindexJob>(CLAIM_BY_ID_PG_SQL)
        .bind(owner)
        .bind(lease_expires_at)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(now)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn pg_advance_checkpoint(
    pool: &PgPool,
    id: i64,
    owner: &str,
    last_processed_id: i64,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let result = sqlx::query(ADVANCE_CHECKPOINT_SQL)
        .bind(last_processed_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn pg_assert_lease_held(
    pool: &PgPool,
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

pub(super) async fn pg_advance_to_completed(
    pool: &PgPool,
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

pub(super) async fn pg_mark_failed(
    pool: &PgPool,
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

pub(super) async fn pg_abort(
    pool: &PgPool,
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

pub(super) async fn pg_reclaim_expired_leases(
    pool: &PgPool,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(RECLAIM_EXPIRED_LEASES_SQL)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

pub(super) async fn pg_list_running(pool: &PgPool) -> Result<Vec<ReindexJob>, StorageError> {
    let sql = format!(
        "SELECT {SELECT_REINDEX_JOB_COLUMNS} \
         FROM reindex_jobs \
         WHERE state IN ('pending', 'running') \
         ORDER BY id ASC"
    );
    sqlx::query_as::<_, ReindexJob>(&sql)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn pg_find_by_id(
    pool: &PgPool,
    id: i64,
) -> Result<Option<ReindexJob>, StorageError> {
    let sql =
        format!("SELECT {SELECT_REINDEX_JOB_COLUMNS} FROM reindex_jobs WHERE id = $1 LIMIT 1");
    sqlx::query_as::<_, ReindexJob>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn pg_find_active_by_target(
    pool: &PgPool,
    target: &str,
) -> Result<Option<ReindexJob>, StorageError> {
    let sql = format!(
        "SELECT {SELECT_REINDEX_JOB_COLUMNS} \
         FROM reindex_jobs \
         WHERE target = $1 AND state IN ('pending', 'running') \
         LIMIT 1"
    );
    sqlx::query_as::<_, ReindexJob>(&sql)
        .bind(target)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
}
