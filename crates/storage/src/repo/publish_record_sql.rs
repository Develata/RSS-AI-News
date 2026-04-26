use sqlx::SqlitePool;

use crate::{ClaimRequest, StorageError};

use super::publish_record::ClaimedPublishRecord;

pub(super) async fn claim_publish(
    pool: &SqlitePool,
    request: &ClaimRequest,
    from: &str,
    to: &str,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    sqlx::query_as::<_, ClaimedPublishRecord>(CLAIM_PUBLISH_SQL)
        .bind(to)
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
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
WHERE id = ?
"#;

const CLAIM_PUBLISH_SQL: &str = r#"
UPDATE publish_records
SET state = ?, lease_owner = ?, lease_expires_at = ?,
    attempt_count = attempt_count + 1, updated_at = ?
WHERE id IN (
    SELECT id FROM publish_records
    WHERE state = ?
      AND (lease_expires_at IS NULL OR lease_expires_at < ?)
      AND attempt_count < ?
    ORDER BY id ASC
    LIMIT ?
)
RETURNING id, idempotency_key, category_key, report_date, target_timezone,
          render_version, selection_policy_version, state, remote_target,
          attempt_count
"#;

pub(super) const ADVANCE_SNAPSHOT_SQL: &str = "UPDATE publish_records SET state = ?, snapshot_frozen_at = ?, local_path = COALESCE(?, local_path), remote_target = COALESCE(?, remote_target), commit_sha = COALESCE(?, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = ? WHERE id = ? AND lease_owner = ? AND state = ?";
pub(super) const ADVANCE_RENDERED_SQL: &str = "UPDATE publish_records SET state = ?, rendered_at = ?, local_path = COALESCE(?, local_path), remote_target = COALESCE(?, remote_target), commit_sha = COALESCE(?, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = ? WHERE id = ? AND lease_owner = ? AND state = ?";
pub(super) const ADVANCE_LOCAL_SQL: &str = "UPDATE publish_records SET state = ?, local_stored_at = ?, local_path = COALESCE(?, local_path), remote_target = COALESCE(?, remote_target), commit_sha = COALESCE(?, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = ? WHERE id = ? AND lease_owner = ? AND state = ?";
pub(super) const ADVANCE_REMOTE_SQL: &str = "UPDATE publish_records SET state = ?, remote_published_at = ?, local_path = COALESCE(?, local_path), remote_target = COALESCE(?, remote_target), commit_sha = COALESCE(?, commit_sha), lease_owner = NULL, lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = ? WHERE id = ? AND lease_owner = ? AND state = ?";
pub(super) const RELEASE_PUBLISH_FAILURE_SQL: &str = "UPDATE publish_records SET lease_owner = NULL, lease_expires_at = NULL, last_error = ?, last_error_kind = ?, updated_at = ? WHERE id = ? AND lease_owner = ?";
