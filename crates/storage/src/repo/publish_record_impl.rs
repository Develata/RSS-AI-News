//! [`PublishRecordRepository`] trait 实装。
//!
//! W11-P3-C-4：所有方法按 backend `match` 分发到 sqlite_*/pg_* 私有 helper；
//! SQL 跨方言完全等价（const 集中在 [`super::publish_record_sql`]）；claim
//! 路径在 PG 加 `FOR UPDATE SKIP LOCKED`（§6.4 契约）。

use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, StorageError, StoragePool, classify_db_error};

use super::{
    publish_record::{
        ClaimedPublishRecord, NewPublishRecord, PublishAdvanceExtras, PublishRecord,
        PublishRecordRepo, PublishRecordRepository, PublishState, PublishTimestampField,
        TerminalAdvanceOutcome, TerminalAdvanceStatus,
    },
    publish_record_sql::{
        ADVANCE_LOCAL_SQL, ADVANCE_REMOTE_SQL, ADVANCE_RENDERED_SQL, ADVANCE_SNAPSHOT_SQL,
        CREATE_IF_NEW_SQL, PROMOTE_ARTICLE_PUBLISHED_SQL, PROMOTE_ARTICLES_PUBLISHED_BATCH_PG_SQL,
        RECLAIM_PUBLISH_LEASES_SQL, RELEASE_PERMANENT_FAILURE_SQL, RELEASE_PUBLISH_FAILURE_SQL,
        SELECT_PUBLISH_RECORD_BY_ID, SELECT_PUBLISH_RECORD_BY_IDEMPOTENCY_KEY, claim_publish_pg,
        claim_publish_sqlite,
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

    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match self.storage_pool() {
            StoragePool::Sqlite(p) => {
                sqlite_release_retryable_failure(p, id, owner, error, kind, now).await
            }
            StoragePool::Postgres(p) => {
                pg_release_retryable_failure(p, id, owner, error, kind, now).await
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

async fn sqlite_release_retryable_failure(
    pool: &SqlitePool,
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
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
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
    let result = sqlx::query(RECLAIM_PUBLISH_LEASES_SQL)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
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
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
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
