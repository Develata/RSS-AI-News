//! [`ReindexJobRepository`] trait 实装：按 backend `match &self.pool` 分发。
//!
//! W11-P3-C-2：claim 路径在 PG 加 `FOR UPDATE SKIP LOCKED`（§6.4 契约）。
//! 实装层超 800 行软上限，按方言再拆：SQLite helper 见
//! [`super::reindex_job_impl_sqlite`]，PostgreSQL helper 见
//! [`super::reindex_job_impl_pg`]；SQL const 集中在 [`super::reindex_job_sql`]。

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::{StorageError, StoragePool};

use super::reindex_job::{
    ClaimedReindexJob, FinishReindexTxOutcome, ReindexJob, ReindexJobRepo, ReindexJobRepository,
    StartReindexTxOutcome,
};
use super::reindex_job_impl_pg::{
    pg_abort, pg_advance_checkpoint, pg_advance_to_completed, pg_assert_lease_held, pg_claim_by_id,
    pg_claim_pending, pg_complete_without_claim, pg_find_active_by_target, pg_find_by_id,
    pg_finish_reindex_tx, pg_insert_pending, pg_list_running, pg_mark_failed,
    pg_reclaim_expired_leases, pg_start_reindex_tx,
};
use super::reindex_job_impl_sqlite::{
    sqlite_abort, sqlite_advance_checkpoint, sqlite_advance_to_completed, sqlite_assert_lease_held,
    sqlite_claim_by_id, sqlite_claim_pending, sqlite_complete_without_claim,
    sqlite_find_active_by_target, sqlite_find_by_id, sqlite_finish_reindex_tx,
    sqlite_insert_pending, sqlite_list_running, sqlite_mark_failed, sqlite_reclaim_expired_leases,
    sqlite_start_reindex_tx,
};

#[async_trait]
impl ReindexJobRepository for ReindexJobRepo {
    async fn start_reindex_tx(
        &self,
        rule_kind: &str,
        rule_version_tag: &str,
        rule_description: &str,
        rule_payload_sha256: &str,
        target: &str,
        now: OffsetDateTime,
    ) -> Result<StartReindexTxOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_start_reindex_tx(
                    p,
                    rule_kind,
                    rule_version_tag,
                    rule_description,
                    rule_payload_sha256,
                    target,
                    now,
                )
                .await
            }
            StoragePool::Postgres(p) => {
                pg_start_reindex_tx(
                    p,
                    rule_kind,
                    rule_version_tag,
                    rule_description,
                    rule_payload_sha256,
                    target,
                    now,
                )
                .await
            }
        }
    }

    async fn complete_without_claim(
        &self,
        id: i64,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_complete_without_claim(p, id, finished_at).await,
            StoragePool::Postgres(p) => pg_complete_without_claim(p, id, finished_at).await,
        }
    }

    async fn finish_reindex_tx(
        &self,
        job_id: i64,
        owner: &str,
        rule_version_id: i64,
        rule_kind: &str,
        finished_at: OffsetDateTime,
    ) -> Result<FinishReindexTxOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_finish_reindex_tx(p, job_id, owner, rule_version_id, rule_kind, finished_at)
                    .await
            }
            StoragePool::Postgres(p) => {
                pg_finish_reindex_tx(p, job_id, owner, rule_version_id, rule_kind, finished_at)
                    .await
            }
        }
    }

    async fn insert_pending(
        &self,
        target: &str,
        rule_version_id: i64,
        now: OffsetDateTime,
    ) -> Result<i64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_insert_pending(p, target, rule_version_id, now).await,
            StoragePool::Postgres(p) => pg_insert_pending(p, target, rule_version_id, now).await,
        }
    }

    async fn claim_pending(
        &self,
        owner: &str,
        now: OffsetDateTime,
        lease_expires_at: OffsetDateTime,
    ) -> Result<Option<ClaimedReindexJob>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_claim_pending(p, owner, now, lease_expires_at).await,
            StoragePool::Postgres(p) => pg_claim_pending(p, owner, now, lease_expires_at).await,
        }
    }

    async fn claim_by_id(
        &self,
        id: i64,
        owner: &str,
        now: OffsetDateTime,
        lease_expires_at: OffsetDateTime,
    ) -> Result<Option<ClaimedReindexJob>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_claim_by_id(p, id, owner, now, lease_expires_at).await,
            StoragePool::Postgres(p) => pg_claim_by_id(p, id, owner, now, lease_expires_at).await,
        }
    }

    async fn advance_checkpoint(
        &self,
        id: i64,
        owner: &str,
        last_processed_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_advance_checkpoint(p, id, owner, last_processed_id, now).await
            }
            StoragePool::Postgres(p) => {
                pg_advance_checkpoint(p, id, owner, last_processed_id, now).await
            }
        }
    }

    async fn assert_lease_held(
        &self,
        id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_assert_lease_held(p, id, owner, now).await,
            StoragePool::Postgres(p) => pg_assert_lease_held(p, id, owner, now).await,
        }
    }

    async fn advance_to_completed(
        &self,
        id: i64,
        owner: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_advance_to_completed(p, id, owner, finished_at).await,
            StoragePool::Postgres(p) => pg_advance_to_completed(p, id, owner, finished_at).await,
        }
    }

    async fn mark_failed(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_mark_failed(p, id, owner, error, finished_at).await,
            StoragePool::Postgres(p) => pg_mark_failed(p, id, owner, error, finished_at).await,
        }
    }

    async fn abort(
        &self,
        id: i64,
        aborted_reason: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_abort(p, id, aborted_reason, finished_at).await,
            StoragePool::Postgres(p) => pg_abort(p, id, aborted_reason, finished_at).await,
        }
    }

    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_reclaim_expired_leases(p, now).await,
            StoragePool::Postgres(p) => pg_reclaim_expired_leases(p, now).await,
        }
    }

    async fn list_running(&self) -> Result<Vec<ReindexJob>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_list_running(p).await,
            StoragePool::Postgres(p) => pg_list_running(p).await,
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<ReindexJob>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_by_id(p, id).await,
            StoragePool::Postgres(p) => pg_find_by_id(p, id).await,
        }
    }

    async fn find_active_by_target(
        &self,
        target: &str,
    ) -> Result<Option<ReindexJob>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_active_by_target(p, target).await,
            StoragePool::Postgres(p) => pg_find_active_by_target(p, target).await,
        }
    }
}
