use async_trait::async_trait;
use time::OffsetDateTime;

use crate::{ClaimRequest, StorageError, classify_sqlite_error};

use super::{
    publish_record::{
        ClaimedPublishRecord, NewPublishRecord, PublishAdvanceExtras, PublishRecord,
        PublishRecordRepository, PublishState, PublishTimestampField, SqlitePublishRecordRepo,
    },
    publish_record_sql::{
        ADVANCE_LOCAL_SQL, ADVANCE_REMOTE_SQL, ADVANCE_RENDERED_SQL, ADVANCE_SNAPSHOT_SQL,
        RELEASE_PUBLISH_FAILURE_SQL, SELECT_PUBLISH_RECORD_BY_ID, claim_publish,
    },
};

#[async_trait]
impl PublishRecordRepository for SqlitePublishRecordRepo {
    async fn create_if_new(&self, item: &NewPublishRecord) -> Result<Option<i64>, StorageError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO publish_records (
                idempotency_key, category_key, report_date, target_timezone,
                render_version, selection_policy_version, remote_target
            )
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(idempotency_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(&item.idempotency_key)
        .bind(&item.category_key)
        .bind(&item.report_date)
        .bind(&item.target_timezone)
        .bind(item.render_version)
        .bind(item.selection_policy_version)
        .bind(&item.remote_target)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| classify_sqlite_error(error, "publish_records", &item.idempotency_key))
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<PublishRecord>, StorageError> {
        sqlx::query_as::<_, PublishRecord>(SELECT_PUBLISH_RECORD_BY_ID)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::from)
    }

    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<PublishRecord>, StorageError> {
        sqlx::query_as::<_, PublishRecord>(
            &SELECT_PUBLISH_RECORD_BY_ID.replace("WHERE id = ?", "WHERE idempotency_key = ?"),
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::from)
    }

    async fn claim_pending_for_freeze(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        claim_publish(&self.pool, request, "pending", "snapshot_frozen").await
    }

    async fn claim_frozen_for_render(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        claim_publish(&self.pool, request, "snapshot_frozen", "rendered").await
    }

    async fn claim_rendered_for_local_store(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        claim_publish(&self.pool, request, "rendered", "stored_local").await
    }

    async fn claim_local_for_remote_publish(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        claim_publish(&self.pool, request, "stored_local", "published_remote").await
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
        let sql = match timestamp_field {
            PublishTimestampField::SnapshotFrozenAt => ADVANCE_SNAPSHOT_SQL,
            PublishTimestampField::RenderedAt => ADVANCE_RENDERED_SQL,
            PublishTimestampField::LocalStoredAt => ADVANCE_LOCAL_SQL,
            PublishTimestampField::RemotePublishedAt => ADVANCE_REMOTE_SQL,
        };
        let result = sqlx::query(sql)
            .bind(to.as_str())
            .bind(now)
            .bind(&extras.local_path)
            .bind(&extras.remote_target)
            .bind(&extras.commit_sha)
            .bind(now)
            .bind(id)
            .bind(owner)
            .bind(from.as_str())
            .execute(&self.pool)
            .await
            .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(RELEASE_PUBLISH_FAILURE_SQL)
            .bind(error)
            .bind(kind)
            .bind(now)
            .bind(id)
            .bind(owner)
            .execute(&self.pool)
            .await
            .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            "UPDATE publish_records SET state = 'failed', lease_owner = NULL, lease_expires_at = NULL, last_error = ?, last_error_kind = ?, updated_at = ? WHERE id = ? AND lease_owner = ?",
        )
        .bind(error)
        .bind(kind)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE publish_records
            SET lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
            WHERE lease_expires_at IS NOT NULL
              AND lease_expires_at < ?
              AND state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local')
            "#,
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected())
    }
}
