use async_trait::async_trait;
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, StorageError, classify_sqlite_error};

#[derive(Debug, Clone)]
pub struct NewFeedEntry {
    pub source_id: i64,
    pub feed_entry_uid: String,
    pub normalized_link: String,
    pub link_hash: String,
    pub title_raw: String,
    pub summary_raw: Option<String>,
    pub published_at: Option<OffsetDateTime>,
    pub discovered_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct FeedEntry {
    pub id: i64,
    pub source_id: i64,
    pub feed_entry_uid: String,
    pub normalized_link: String,
    pub link_hash: String,
    pub title_raw: String,
    pub summary_raw: Option<String>,
    pub published_at: Option<OffsetDateTime>,
    pub discovered_at: OffsetDateTime,
    pub state: String,
    pub dedup_decision: Option<String>,
    pub article_id: Option<i64>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub last_error_kind: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClaimedFeedEntry {
    pub id: i64,
    pub source_id: i64,
    pub normalized_link: String,
    pub link_hash: String,
    pub title_raw: String,
    pub discovered_at: OffsetDateTime,
    pub attempt_count: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ResetFailedFilter {
    pub date_from: Option<OffsetDateTime>,
    pub date_to: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Default)]
pub struct ResetFailedOutcome {
    pub examined: u32,
    pub reset: u32,
}

#[derive(Debug, Clone, FromRow)]
pub struct LinkHashReindexCandidate {
    pub id: i64,
    pub normalized_link: String,
    pub link_hash: String,
}

#[async_trait]
pub trait FeedEntryRepository: Send + Sync {
    async fn insert_if_new(&self, entry: &NewFeedEntry) -> Result<Option<i64>, StorageError>;
    async fn exists_by_link_hash(&self, link_hash: &str) -> Result<bool, StorageError>;
    async fn find_by_id(&self, id: i64) -> Result<Option<FeedEntry>, StorageError>;
    async fn claim_pending_fetch(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedFeedEntry>, StorageError>;
    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError>;
    async fn release_dedup_skipped(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        decision: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn release_fallback_persisted(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn reset_failed_in_window(
        &self,
        filter: &ResetFailedFilter,
    ) -> Result<ResetFailedOutcome, StorageError>;
    async fn list_for_link_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<LinkHashReindexCandidate>, StorageError>;
    async fn update_link_hash(&self, id: i64, new_link_hash: &str) -> Result<bool, StorageError>;
}

#[derive(Debug, Clone)]
pub struct SqliteFeedEntryRepo {
    pool: SqlitePool,
}

impl SqliteFeedEntryRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl FeedEntryRepository for SqliteFeedEntryRepo {
    async fn insert_if_new(&self, entry: &NewFeedEntry) -> Result<Option<i64>, StorageError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO feed_entries (
                source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
                summary_raw, published_at, discovered_at, state, dedup_decision
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending_fetch', 'fresh')
            ON CONFLICT(source_id, feed_entry_uid) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(entry.source_id)
        .bind(&entry.feed_entry_uid)
        .bind(&entry.normalized_link)
        .bind(&entry.link_hash)
        .bind(&entry.title_raw)
        .bind(&entry.summary_raw)
        .bind(entry.published_at)
        .bind(entry.discovered_at)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "feed_entries",
                format!("{}/{}", entry.source_id, entry.feed_entry_uid),
            )
        })
    }

    async fn exists_by_link_hash(&self, link_hash: &str) -> Result<bool, StorageError> {
        let exists = sqlx::query_scalar::<_, i32>(
            "SELECT CASE WHEN EXISTS(SELECT 1 FROM feed_entries WHERE link_hash = ?) THEN 1 ELSE 0 END",
        )
        .bind(link_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(exists != 0)
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<FeedEntry>, StorageError> {
        sqlx::query_as::<_, FeedEntry>(SELECT_FEED_ENTRY_BY_ID)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::from)
    }

    async fn claim_pending_fetch(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedFeedEntry>, StorageError> {
        sqlx::query_as::<_, ClaimedFeedEntry>(
            r#"
            UPDATE feed_entries
            SET state = 'fetching',
                lease_owner = ?,
                lease_expires_at = ?,
                attempt_count = attempt_count + 1,
                updated_at = ?
            WHERE id IN (
                SELECT id FROM feed_entries
                WHERE state = 'pending_fetch'
                  AND (lease_expires_at IS NULL OR lease_expires_at < ?)
                  AND attempt_count < ?
                ORDER BY discovered_at ASC
                LIMIT ?
            )
            RETURNING id, source_id, normalized_link, link_hash, title_raw,
                      discovered_at, attempt_count
            "#,
        )
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
        .bind(request.now)
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::from)
    }

    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE feed_entries
            SET state = 'persisted', article_id = ?, lease_owner = NULL,
                lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL,
                updated_at = ?
            WHERE id = ? AND lease_owner = ?
            "#,
        )
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
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
        release_feed_failure(&self.pool, id, owner, error, kind, now, "pending_fetch").await
    }

    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        release_feed_failure(&self.pool, id, owner, error, kind, now, "failed").await
    }

    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        // 设计 §5.5 写明 reclaim 不改 state，但 §5.1 只领取 pending_fetch。
        // 这里按 W4b 指令采用方案 A：过期 fetching/extracting 回到 pending_fetch。
        let result = sqlx::query(
            r#"
            UPDATE feed_entries
            SET state = 'pending_fetch',
                lease_owner = NULL,
                lease_expires_at = NULL,
                updated_at = ?
            WHERE lease_expires_at IS NOT NULL
              AND lease_expires_at < ?
              AND state IN ('fetching', 'extracting')
            "#,
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected())
    }

    async fn release_dedup_skipped(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        decision: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE feed_entries
            SET state = 'dedup_skipped',
                dedup_decision = ?,
                article_id = ?,
                lease_owner = NULL,
                lease_expires_at = NULL,
                last_error = NULL,
                last_error_kind = NULL,
                updated_at = ?
            WHERE id = ? AND lease_owner = ?
            "#,
        )
        .bind(decision)
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn release_fallback_persisted(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE feed_entries
            SET state = 'fallback_persisted',
                article_id = ?,
                lease_owner = NULL,
                lease_expires_at = NULL,
                last_error = NULL,
                last_error_kind = NULL,
                updated_at = ?
            WHERE id = ? AND lease_owner = ?
            "#,
        )
        .bind(article_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn reset_failed_in_window(
        &self,
        filter: &ResetFailedFilter,
    ) -> Result<ResetFailedOutcome, StorageError> {
        let examined = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM feed_entries
            WHERE (?1 IS NULL OR created_at >= ?1)
              AND (?2 IS NULL OR created_at < ?2)
            "#,
        )
        .bind(filter.date_from)
        .bind(filter.date_to)
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::from)?;

        let result = sqlx::query(
            r#"
            UPDATE feed_entries
            SET state = 'discovered',
                attempt_count = 0,
                last_error = NULL,
                last_error_kind = NULL,
                lease_owner = NULL,
                lease_expires_at = NULL,
                updated_at = CURRENT_TIMESTAMP
            WHERE state = 'failed'
              AND (?1 IS NULL OR created_at >= ?1)
              AND (?2 IS NULL OR created_at < ?2)
            "#,
        )
        .bind(filter.date_from)
        .bind(filter.date_to)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;

        Ok(ResetFailedOutcome {
            examined: u32::try_from(examined).unwrap_or(u32::MAX),
            reset: u32::try_from(result.rows_affected()).unwrap_or(u32::MAX),
        })
    }

    async fn list_for_link_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<LinkHashReindexCandidate>, StorageError> {
        sqlx::query_as::<_, LinkHashReindexCandidate>(
            r#"
            SELECT id, normalized_link, link_hash
            FROM feed_entries
            WHERE id > ?
            ORDER BY id ASC
            LIMIT ?
            "#,
        )
        .bind(after_id)
        .bind(i64::from(batch_size))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::from)
    }

    async fn update_link_hash(&self, id: i64, new_link_hash: &str) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE feed_entries
            SET link_hash = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
        )
        .bind(new_link_hash)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }
}

async fn release_feed_failure(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    now: OffsetDateTime,
    state: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(
        r#"
        UPDATE feed_entries
        SET state = ?, lease_owner = NULL, lease_expires_at = NULL,
            last_error = ?, last_error_kind = ?, updated_at = ?
        WHERE id = ? AND lease_owner = ?
        "#,
    )
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

const SELECT_FEED_ENTRY_BY_ID: &str = r#"
SELECT id, source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
       summary_raw, published_at, discovered_at, state, dedup_decision,
       article_id, lease_owner, lease_expires_at, attempt_count, last_error,
       last_error_kind, created_at, updated_at
FROM feed_entries
WHERE id = ?
"#;
