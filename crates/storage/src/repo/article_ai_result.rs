use async_trait::async_trait;
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, StorageError, classify_sqlite_error};

#[derive(Debug, Clone)]
pub struct NewAiResult {
    pub article_id: i64,
    pub prompt_version: i64,
    pub output_schema_version: i64,
    pub model_id: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClaimedAiResult {
    pub id: i64,
    pub article_id: i64,
    pub prompt_version: i64,
    pub output_schema_version: i64,
    pub model_id: String,
}

#[derive(Debug, Clone)]
pub struct AiSuccessOutcome {
    pub summary: String,
    pub tags_json: String,
    pub importance_score: Option<i32>,
    pub keep_decision: Option<bool>,
    pub raw_response_artifact_id: Option<i64>,
    pub tokens_in: Option<i32>,
    pub tokens_out: Option<i32>,
    pub cost_micro_usd: Option<i64>,
    pub latency_ms: Option<i32>,
}

#[async_trait]
pub trait ArticleAiResultRepository: Send + Sync {
    async fn insert_pending(&self, item: &NewAiResult) -> Result<Option<i64>, StorageError>;
    async fn claim_pending(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedAiResult>, StorageError>;
    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        outcome: AiSuccessOutcome,
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
}

#[derive(Debug, Clone)]
pub struct SqliteArticleAiResultRepo {
    pool: SqlitePool,
}

impl SqliteArticleAiResultRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ArticleAiResultRepository for SqliteArticleAiResultRepo {
    async fn insert_pending(&self, item: &NewAiResult) -> Result<Option<i64>, StorageError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO article_ai_results (
                article_id, prompt_version, output_schema_version, model_id, state
            )
            VALUES (?, ?, ?, ?, 'pending')
            ON CONFLICT(article_id, prompt_version, output_schema_version, model_id) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "article_ai_results",
                format!(
                    "{}/{}/{}/{}",
                    item.article_id, item.prompt_version, item.output_schema_version, item.model_id
                ),
            )
        })
    }

    async fn claim_pending(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedAiResult>, StorageError> {
        sqlx::query_as::<_, ClaimedAiResult>(
            r#"
            UPDATE article_ai_results
            SET state = 'running',
                lease_owner = ?,
                lease_expires_at = ?,
                attempt_count = attempt_count + 1,
                started_at = COALESCE(started_at, ?),
                updated_at = ?
            WHERE id IN (
                SELECT id FROM article_ai_results
                WHERE state = 'pending'
                  AND (lease_expires_at IS NULL OR lease_expires_at < ?)
                  AND attempt_count < ?
                ORDER BY id ASC
                LIMIT ?
            )
            RETURNING id, article_id, prompt_version, output_schema_version, model_id
            "#,
        )
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
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
        outcome: AiSuccessOutcome,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let state = if outcome.keep_decision == Some(false) {
            "filtered"
        } else {
            "succeeded"
        };
        let keep_decision = outcome.keep_decision.map(i32::from);
        let result = sqlx::query(
            r#"
            UPDATE article_ai_results
            SET state = ?, summary = ?, tags_json = ?, importance_score = ?,
                keep_decision = ?, raw_response_artifact_id = ?, tokens_in = ?,
                tokens_out = ?, cost_micro_usd = ?, latency_ms = ?,
                lease_owner = NULL, lease_expires_at = NULL,
                last_error = NULL, last_error_kind = NULL,
                completed_at = ?, updated_at = ?
            WHERE id = ? AND lease_owner = ?
            "#,
        )
        .bind(state)
        .bind(outcome.summary)
        .bind(outcome.tags_json)
        .bind(outcome.importance_score)
        .bind(keep_decision)
        .bind(outcome.raw_response_artifact_id)
        .bind(outcome.tokens_in)
        .bind(outcome.tokens_out)
        .bind(outcome.cost_micro_usd)
        .bind(outcome.latency_ms)
        .bind(now)
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
        release_ai_failure(&self.pool, id, owner, error, kind, now, "pending").await
    }

    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        release_ai_failure(&self.pool, id, owner, error, kind, now, "permanent_failed").await
    }

    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        // 同 feed_entries：为保证下一轮 claim 可见，running 过期行回到 pending。
        let result = sqlx::query(
            r#"
            UPDATE article_ai_results
            SET state = 'pending',
                lease_owner = NULL,
                lease_expires_at = NULL,
                updated_at = ?
            WHERE lease_expires_at IS NOT NULL
              AND lease_expires_at < ?
              AND state = 'running'
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

async fn release_ai_failure(
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
        UPDATE article_ai_results
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
