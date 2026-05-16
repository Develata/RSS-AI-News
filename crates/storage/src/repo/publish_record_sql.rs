use sqlx::SqlitePool;

use crate::{ClaimRequest, StorageError};

use super::publish_record::ClaimedPublishRecord;

pub(super) async fn claim_publish(
    pool: &SqlitePool,
    request: &ClaimRequest,
    from: &str,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    sqlx::query_as::<_, ClaimedPublishRecord>(CLAIM_PUBLISH_SQL)
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

const CLAIM_PUBLISH_SQL: &str = r#"
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

pub(super) const ADVANCE_SNAPSHOT_SQL: &str = "UPDATE publish_records SET state = $1, snapshot_frozen_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_RENDERED_SQL: &str = "UPDATE publish_records SET state = $1, rendered_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_LOCAL_SQL: &str = "UPDATE publish_records SET state = $1, local_stored_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_REMOTE_SQL: &str = "UPDATE publish_records SET state = $1, remote_published_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const RELEASE_PUBLISH_FAILURE_SQL: &str = "UPDATE publish_records SET lease_owner = NULL, lease_expires_at = NULL, last_error = $1, last_error_kind = $2, updated_at = $3 WHERE id = $4 AND lease_owner = $5";
