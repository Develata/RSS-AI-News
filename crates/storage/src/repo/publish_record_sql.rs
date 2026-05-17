//! publish_records 共享 SQL 字符串 + claim 辅助。
//!
//! W11-P3-C-4：所有 const SQL 跨方言完全等价（已用 `$N` 占位符 +
//! ON CONFLICT + RETURNING），仅 `claim_publish` 子查询在 PG 必须加
//! `FOR UPDATE SKIP LOCKED`（§6.4 契约），故按 backend 分裂。

use sqlx::{PgPool, SqlitePool};

use crate::{ClaimRequest, StorageError};

use super::publish_record::ClaimedPublishRecord;

pub(super) async fn claim_publish_sqlite(
    pool: &SqlitePool,
    request: &ClaimRequest,
    from: &str,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    sqlx::query_as::<_, ClaimedPublishRecord>(CLAIM_PUBLISH_SQLITE_SQL)
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
        .bind(from)
        .bind(request.now)
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn claim_publish_pg(
    pool: &PgPool,
    request: &ClaimRequest,
    from: &str,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    sqlx::query_as::<_, ClaimedPublishRecord>(CLAIM_PUBLISH_PG_SQL)
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
        .bind(from)
        .bind(request.now)
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) const SELECT_PUBLISH_RECORD_BY_ID: &str = r#"
SELECT id, idempotency_key, category_key, report_date, target_timezone,
       render_version, selection_policy_version, state, snapshot_frozen_at,
       rendered_at, local_stored_at, remote_published_at, local_path,
       remote_target, commit_sha, lease_owner, lease_expires_at, attempt_count,
       last_error, last_error_kind, created_at, updated_at
FROM publish_records
WHERE id = $1
"#;

pub(super) const SELECT_PUBLISH_RECORD_BY_IDEMPOTENCY_KEY: &str = r#"
SELECT id, idempotency_key, category_key, report_date, target_timezone,
       render_version, selection_policy_version, state, snapshot_frozen_at,
       rendered_at, local_stored_at, remote_published_at, local_path,
       remote_target, commit_sha, lease_owner, lease_expires_at, attempt_count,
       last_error, last_error_kind, created_at, updated_at
FROM publish_records
WHERE idempotency_key = $1
"#;

pub(super) const CREATE_IF_NEW_SQL: &str = r#"
INSERT INTO publish_records (
    idempotency_key, category_key, report_date, target_timezone,
    render_version, selection_policy_version, remote_target
)
VALUES ($1, $2, $3, $4, $5, $6, $7)
ON CONFLICT(idempotency_key) DO NOTHING
RETURNING id
"#;

/// SQLite claim：子查询无 `FOR UPDATE`（语法不支持，整库写锁兜底）。
const CLAIM_PUBLISH_SQLITE_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = $1, lease_expires_at = $2,
    attempt_count = attempt_count + 1, updated_at = $3
WHERE id IN (
    SELECT id FROM publish_records
    WHERE state = $4
      AND (lease_expires_at IS NULL OR lease_expires_at < $5)
      AND attempt_count < $6
    ORDER BY id ASC
    LIMIT $7
)
RETURNING id, idempotency_key, category_key, report_date, target_timezone,
          render_version, selection_policy_version, state, remote_target,
          attempt_count
"#;

/// PG claim：§6.4 契约——子查询 `FOR UPDATE SKIP LOCKED`，让 publish flow
/// 多 worker 并发 claim 同状态的多个 publish_records 时各自拿到不同候选；
/// 否则会序列化等待 row lock，等价单 worker。
const CLAIM_PUBLISH_PG_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = $1, lease_expires_at = $2,
    attempt_count = attempt_count + 1, updated_at = $3
WHERE id IN (
    SELECT id FROM publish_records
    WHERE state = $4
      AND (lease_expires_at IS NULL OR lease_expires_at < $5)
      AND attempt_count < $6
    ORDER BY id ASC
    LIMIT $7
    FOR UPDATE SKIP LOCKED
)
RETURNING id, idempotency_key, category_key, report_date, target_timezone,
          render_version, selection_policy_version, state, remote_target,
          attempt_count
"#;

pub(super) const ADVANCE_SNAPSHOT_SQL: &str = "UPDATE publish_records SET state = $1, snapshot_frozen_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_RENDERED_SQL: &str = "UPDATE publish_records SET state = $1, rendered_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_LOCAL_SQL: &str = "UPDATE publish_records SET state = $1, local_stored_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_REMOTE_SQL: &str = "UPDATE publish_records SET state = $1, remote_published_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const RELEASE_PUBLISH_FAILURE_SQL: &str = "UPDATE publish_records SET lease_owner = NULL, lease_expires_at = NULL, last_error = $1, last_error_kind = $2, updated_at = $3 WHERE id = $4 AND lease_owner = $5";
pub(super) const RELEASE_PERMANENT_FAILURE_SQL: &str = "UPDATE publish_records SET state = 'failed', lease_owner = NULL, lease_expires_at = NULL, last_error = $1, last_error_kind = $2, updated_at = $3 WHERE id = $4 AND lease_owner = $5";
pub(super) const RECLAIM_PUBLISH_LEASES_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = NULL, lease_expires_at = NULL, updated_at = $1
WHERE lease_expires_at IS NOT NULL
  AND lease_expires_at < $2
  AND state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local')
"#;
pub(super) const PROMOTE_ARTICLE_PUBLISHED_SQL: &str = "UPDATE articles SET state = 'published', updated_at = $1 WHERE id = $2 AND state = 'ready_for_publish'";
