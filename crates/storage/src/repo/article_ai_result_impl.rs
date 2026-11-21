//! [`ArticleAiResultRepository`] trait 实装。
//!
//! W11-P3-E-3：每方法按 backend `match &self.pool` 分发到 sqlite_*/pg_* 私有
//! helper；SQL const 集中在 [`super::article_ai_result_sql`]；claim 路径在 PG 加
//! `FOR UPDATE SKIP LOCKED`（§6.4 契约），故两侧 SQL 分叉。

use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, ReleaseFailureOutcome, StorageError, StoragePool, classify_db_error};

use super::{sqlite_instant_cutoff, sqlite_stored_deadline};

use super::article_ai_result::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepo, ArticleAiResultRepository,
    ClaimedAiResult, InsertPendingOutcome, NewAiResult, ReleaseSuccessOutcome,
};
use super::article_ai_result_sql::{
    ADVANCE_ARTICLE_FROM_AI_PHASE_SQL, ADVANCE_ARTICLE_TO_AI_PENDING_SQL,
    ADVANCE_PERSISTED_ARTICLE_SQL, CLAIM_AI_PENDING_FRESH_PG_SQL,
    CLAIM_AI_PENDING_FRESH_SQLITE_SQL, CLAIM_AI_PENDING_PG_SQL, CLAIM_AI_PENDING_SQLITE_SQL,
    INSERT_AI_PENDING_SQL, LAST_CLAIM_IS_FRESH_PG_SQL, LAST_CLAIM_IS_FRESH_SQLITE_SQL,
    NEWEST_TASK_IS_FRESH_PG_SQL, NEWEST_TASK_IS_FRESH_SQLITE_SQL, OTHER_SUCCEEDED_AI_EXISTS_SQL,
    RECLAIM_AI_LEASES_SQL, RECLAIM_AI_LEASES_SQLITE_SQL, RELEASE_AI_FAILURE_SQL,
    RELEASE_AI_RETRYABLE_FAILURE_SQL, RELEASE_AI_SUCCESS_SQL, SELECT_AI_IDENTITY_ROW_PG_SQL,
    SELECT_AI_IDENTITY_ROW_SQLITE_SQL, SELECT_ARTICLE_STATE_SQL, TERMINALIZE_EXHAUSTED_AI_SQL,
    TERMINALIZE_EXHAUSTED_AI_SQLITE_SQL,
};

/// `(id, state, keep_decision, importance_score)` of an existing identity row.
type IdentityRow = (i64, String, Option<i32>, Option<i32>);

/// What a conflicting identity row implies for a still-`persisted` article.
enum ConflictTarget {
    /// `pending` / `running`: the task is (or will be) executed.
    AiPending,
    /// `succeeded` / `filtered` and a threshold is known (task_gen).
    Finished {
        id: i64,
        keep_decision: Option<bool>,
        importance_score: Option<i32>,
        min_importance_score: i32,
    },
    /// `permanent_failed`, unknown, or a finished row without a threshold
    /// (backfill): the article stays `persisted`.
    Leave,
}

fn conflict_target(row: Option<IdentityRow>, min_importance_score: Option<i32>) -> ConflictTarget {
    match (row, min_importance_score) {
        (Some((_, state, _, _)), _) if state == "pending" || state == "running" => {
            ConflictTarget::AiPending
        }
        (Some((id, state, keep, score)), Some(min))
            if state == "succeeded" || state == "filtered" =>
        {
            ConflictTarget::Finished {
                id,
                keep_decision: keep.map(|value| value != 0),
                importance_score: score,
                min_importance_score: min,
            }
        }
        _ => ConflictTarget::Leave,
    }
}

fn conflict_outcome(article_state: Option<&str>, advanced: bool) -> InsertPendingOutcome {
    InsertPendingOutcome {
        ai_result_id: None,
        article_advanced: advanced,
        article_already_advanced: article_state != Some("persisted"),
        reconciled: advanced,
    }
}

// ── trait 实现 ─────────────────────────────────────────────────

#[async_trait]
impl ArticleAiResultRepository for ArticleAiResultRepo {
    async fn insert_pending(&self, item: &NewAiResult) -> Result<Option<i64>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_insert_pending(p, item).await,
            StoragePool::Postgres(p) => pg_insert_pending(p, item).await,
        }
    }

    async fn claim_pending(
        &self,
        request: &ClaimRequest,
        category_key: &str,
    ) -> Result<Vec<ClaimedAiResult>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_claim_pending(p, request, category_key).await,
            StoragePool::Postgres(p) => pg_claim_pending(p, request, category_key).await,
        }
    }

    async fn claim_pending_fresh(
        &self,
        request: &ClaimRequest,
        category_key: &str,
        published_since: OffsetDateTime,
    ) -> Result<Vec<ClaimedAiResult>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlx::query_as::<_, ClaimedAiResult>(CLAIM_AI_PENDING_FRESH_SQLITE_SQL)
                    .bind(&request.owner)
                    .bind(sqlite_stored_deadline(request.lease_expires_at))
                    .bind(request.now)
                    .bind(request.now)
                    .bind(sqlite_instant_cutoff(request.now))
                    .bind(i64::from(request.max_attempts))
                    .bind(category_key)
                    .bind(i64::from(request.batch_size))
                    .bind(sqlite_instant_cutoff(published_since))
                    .fetch_all(p)
                    .await
                    .map_err(StorageError::from)
            }
            StoragePool::Postgres(p) => {
                sqlx::query_as::<_, ClaimedAiResult>(CLAIM_AI_PENDING_FRESH_PG_SQL)
                    .bind(&request.owner)
                    .bind(request.lease_expires_at)
                    .bind(request.now)
                    .bind(request.now)
                    .bind(request.now)
                    .bind(i64::from(request.max_attempts))
                    .bind(category_key)
                    .bind(i64::from(request.batch_size))
                    .bind(published_since)
                    .fetch_all(p)
                    .await
                    .map_err(StorageError::from)
            }
        }
    }

    async fn newest_task_is_fresh(
        &self,
        category_key: &str,
        published_since: OffsetDateTime,
    ) -> Result<Option<bool>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlx::query_scalar::<_, Option<bool>>(NEWEST_TASK_IS_FRESH_SQLITE_SQL)
                    .bind(category_key)
                    .bind(sqlite_instant_cutoff(published_since))
                    .fetch_optional(p)
                    .await
                    .map(Option::flatten)
                    .map_err(StorageError::from)
            }
            StoragePool::Postgres(p) => {
                sqlx::query_scalar::<_, Option<bool>>(NEWEST_TASK_IS_FRESH_PG_SQL)
                    .bind(category_key)
                    .bind(published_since)
                    .fetch_optional(p)
                    .await
                    .map(Option::flatten)
                    .map_err(StorageError::from)
            }
        }
    }

    async fn last_claim_is_fresh(
        &self,
        category_key: &str,
        published_since: OffsetDateTime,
    ) -> Result<Option<bool>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlx::query_scalar::<_, Option<bool>>(LAST_CLAIM_IS_FRESH_SQLITE_SQL)
                    .bind(category_key)
                    .bind(sqlite_instant_cutoff(published_since))
                    .fetch_optional(p)
                    .await
                    .map(Option::flatten)
                    .map_err(StorageError::from)
            }
            StoragePool::Postgres(p) => {
                sqlx::query_scalar::<_, Option<bool>>(LAST_CLAIM_IS_FRESH_PG_SQL)
                    .bind(category_key)
                    .bind(published_since)
                    .fetch_optional(p)
                    .await
                    .map(Option::flatten)
                    .map_err(StorageError::from)
            }
        }
    }

    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        outcome: AiSuccessOutcome,
        effective_model_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_success(p, id, owner, outcome, effective_model_id, now).await
            }
            StoragePool::Postgres(p) => {
                pg_release_success(p, id, owner, outcome, effective_model_id, now).await
            }
        }
    }

    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_ai_retryable_failure(p, id, owner, error, kind, max_attempts, now)
                    .await
            }
            StoragePool::Postgres(p) => {
                pg_release_ai_retryable_failure(p, id, owner, error, kind, max_attempts, now).await
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
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_ai_failure(p, id, owner, error, kind, now, "permanent_failed").await
            }
            StoragePool::Postgres(p) => {
                pg_release_ai_failure(p, id, owner, error, kind, now, "permanent_failed").await
            }
        }
    }

    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_reclaim_expired_leases(p, now).await,
            StoragePool::Postgres(p) => pg_reclaim_expired_leases(p, now).await,
        }
    }

    async fn terminalize_exhausted(
        &self,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_terminalize_exhausted(p, max_attempts, now).await,
            StoragePool::Postgres(p) => pg_terminalize_exhausted(p, max_attempts, now).await,
        }
    }

    async fn insert_pending_and_advance_article(
        &self,
        item: &NewAiResult,
        min_importance_score: i32,
        now: OffsetDateTime,
    ) -> Result<InsertPendingOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_insert_pending_and_advance(p, item, min_importance_score, now).await
            }
            StoragePool::Postgres(p) => {
                pg_insert_pending_and_advance(p, item, min_importance_score, now).await
            }
        }
    }

    async fn insert_backfill_pending(
        &self,
        item: &NewAiResult,
        now: OffsetDateTime,
    ) -> Result<InsertPendingOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_insert_backfill_pending(p, item, now).await,
            StoragePool::Postgres(p) => pg_insert_backfill_pending(p, item, now).await,
        }
    }

    async fn release_success_and_advance_article(
        &self,
        id: i64,
        owner: &str,
        outcome: AiSuccessOutcome,
        effective_model_id: &str,
        article_id: i64,
        min_importance_score: i32,
        now: OffsetDateTime,
    ) -> Result<ReleaseSuccessOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_release_success_and_advance(
                    p,
                    id,
                    owner,
                    outcome,
                    effective_model_id,
                    article_id,
                    min_importance_score,
                    now,
                )
                .await
            }
            StoragePool::Postgres(p) => {
                pg_release_success_and_advance(
                    p,
                    id,
                    owner,
                    outcome,
                    effective_model_id,
                    article_id,
                    min_importance_score,
                    now,
                )
                .await
            }
        }
    }
}

// ── SQLite helper ──────────────────────────────────────────────

fn classify_insert_error(error: sqlx::Error, item: &NewAiResult) -> StorageError {
    classify_db_error(
        error,
        "article_ai_results",
        format!(
            "{}/{}/{}/{}",
            item.article_id, item.prompt_version, item.output_schema_version, item.model_id
        ),
    )
}

async fn sqlite_insert_pending(
    pool: &SqlitePool,
    item: &NewAiResult,
) -> Result<Option<i64>, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_AI_PENDING_SQL)
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| classify_insert_error(error, item))
}

async fn sqlite_claim_pending(
    pool: &SqlitePool,
    request: &ClaimRequest,
    category_key: &str,
) -> Result<Vec<ClaimedAiResult>, StorageError> {
    sqlx::query_as::<_, ClaimedAiResult>(CLAIM_AI_PENDING_SQLITE_SQL)
        .bind(&request.owner)
        .bind(sqlite_stored_deadline(request.lease_expires_at))
        .bind(request.now)
        .bind(request.now)
        .bind(sqlite_instant_cutoff(request.now))
        .bind(i64::from(request.max_attempts))
        .bind(category_key)
        .bind(i64::from(request.batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

async fn sqlite_release_success(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    outcome: AiSuccessOutcome,
    effective_model_id: &str,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let state = if outcome.keep_decision == Some(false) {
        "filtered"
    } else {
        "succeeded"
    };
    let keep_decision = outcome.keep_decision.map(i32::from);
    let result = sqlx::query(RELEASE_AI_SUCCESS_SQL)
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
        .bind(effective_model_id)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn sqlite_release_ai_failure(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    now: OffsetDateTime,
    state: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_AI_FAILURE_SQL)
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

async fn sqlite_release_ai_retryable_failure(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<ReleaseFailureOutcome, StorageError> {
    let state = sqlx::query_scalar::<_, String>(RELEASE_AI_RETRYABLE_FAILURE_SQL)
        .bind(i64::from(max_attempts))
        .bind(error)
        .bind(kind)
        .bind(now)
        .bind(id)
        .bind(owner)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(ReleaseFailureOutcome {
        released: state.is_some(),
        exhausted: state.as_deref() == Some("permanent_failed"),
    })
}

async fn sqlite_reclaim_expired_leases(
    pool: &SqlitePool,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(RECLAIM_AI_LEASES_SQLITE_SQL)
        .bind(now)
        .bind(sqlite_instant_cutoff(now))
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

async fn sqlite_terminalize_exhausted(
    pool: &SqlitePool,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(TERMINALIZE_EXHAUSTED_AI_SQLITE_SQL)
        .bind(now)
        .bind(i64::from(max_attempts))
        .bind(sqlite_instant_cutoff(now))
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

async fn sqlite_insert_pending_and_advance(
    pool: &SqlitePool,
    item: &NewAiResult,
    min_importance_score: i32,
    now: OffsetDateTime,
) -> Result<InsertPendingOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let inserted_id = sqlx::query_scalar::<_, i64>(INSERT_AI_PENDING_SQL)
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| classify_insert_error(error, item))?;

    let Some(ai_result_id) = inserted_id else {
        let outcome =
            sqlite_reconcile_conflict(&mut tx, item, Some(min_importance_score), now).await?;
        tx.commit().await.map_err(StorageError::from)?;
        return Ok(outcome);
    };

    let result = sqlx::query(ADVANCE_ARTICLE_TO_AI_PENDING_SQL)
        .bind(now)
        .bind(item.article_id)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;

    if result.rows_affected() == 0 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(InsertPendingOutcome {
            ai_result_id: None,
            article_advanced: false,
            article_already_advanced: true,
            reconciled: false,
        });
    }

    tx.commit().await.map_err(StorageError::from)?;
    Ok(InsertPendingOutcome {
        ai_result_id: Some(ai_result_id),
        article_advanced: true,
        article_already_advanced: false,
        reconciled: false,
    })
}

/// Conflict branch shared by task_gen and backfill (see the trait docs).
/// `min_importance_score = None` (backfill) leaves finished rows alone.
async fn sqlite_reconcile_conflict(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    item: &NewAiResult,
    min_importance_score: Option<i32>,
    now: OffsetDateTime,
) -> Result<InsertPendingOutcome, StorageError> {
    let row = sqlx::query_as::<_, IdentityRow>(SELECT_AI_IDENTITY_ROW_SQLITE_SQL)
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(StorageError::from)?;
    let article_state = sqlx::query_scalar::<_, String>(SELECT_ARTICLE_STATE_SQL)
        .bind(item.article_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(StorageError::from)?;
    if article_state.as_deref() != Some("persisted") {
        return Ok(conflict_outcome(article_state.as_deref(), false));
    }
    let next_state = match conflict_target(row, min_importance_score) {
        ConflictTarget::AiPending => Some("ai_pending"),
        ConflictTarget::Finished {
            id,
            keep_decision,
            importance_score,
            min_importance_score,
        } => sqlite_compute_article_advance(
            tx,
            id,
            item.article_id,
            keep_decision,
            importance_score,
            min_importance_score,
        )
        .await?
        .as_article_state_str(),
        ConflictTarget::Leave => None,
    };
    let Some(next_state) = next_state else {
        return Ok(conflict_outcome(article_state.as_deref(), false));
    };
    let advanced = sqlx::query(ADVANCE_PERSISTED_ARTICLE_SQL)
        .bind(next_state)
        .bind(now)
        .bind(item.article_id)
        .execute(&mut **tx)
        .await
        .map_err(StorageError::from)?
        .rows_affected()
        == 1;
    Ok(conflict_outcome(article_state.as_deref(), advanced))
}

async fn sqlite_insert_backfill_pending(
    pool: &SqlitePool,
    item: &NewAiResult,
    now: OffsetDateTime,
) -> Result<InsertPendingOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let inserted_id = sqlx::query_scalar::<_, i64>(INSERT_AI_PENDING_SQL)
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| classify_insert_error(error, item))?;
    let outcome = match inserted_id {
        Some(ai_result_id) => {
            // Unlike task_gen, the row is kept when the article has moved on.
            let advanced = sqlx::query(ADVANCE_ARTICLE_TO_AI_PENDING_SQL)
                .bind(now)
                .bind(item.article_id)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::from)?
                .rows_affected()
                == 1;
            InsertPendingOutcome {
                ai_result_id: Some(ai_result_id),
                article_advanced: advanced,
                article_already_advanced: !advanced,
                reconciled: false,
            }
        }
        None => sqlite_reconcile_conflict(&mut tx, item, None, now).await?,
    };
    tx.commit().await.map_err(StorageError::from)?;
    Ok(outcome)
}

async fn sqlite_release_success_and_advance(
    pool: &SqlitePool,
    id: i64,
    owner: &str,
    outcome: AiSuccessOutcome,
    effective_model_id: &str,
    article_id: i64,
    min_importance_score: i32,
    now: OffsetDateTime,
) -> Result<ReleaseSuccessOutcome, StorageError> {
    let keep_decision = outcome.keep_decision;
    let importance_score = outcome.importance_score;
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let state = if keep_decision == Some(false) {
        "filtered"
    } else {
        "succeeded"
    };
    let keep_decision_i32 = keep_decision.map(i32::from);
    let result = sqlx::query(RELEASE_AI_SUCCESS_SQL)
        .bind(state)
        .bind(outcome.summary)
        .bind(outcome.tags_json)
        .bind(importance_score)
        .bind(keep_decision_i32)
        .bind(outcome.raw_response_artifact_id)
        .bind(outcome.tokens_in)
        .bind(outcome.tokens_out)
        .bind(outcome.cost_micro_usd)
        .bind(outcome.latency_ms)
        .bind(effective_model_id)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;

    if result.rows_affected() == 0 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(ReleaseSuccessOutcome {
            released: false,
            article_advance: AiCompleteArticleAdvance::NoChange,
        });
    }

    let current_article_state = sqlx::query_scalar::<_, String>(SELECT_ARTICLE_STATE_SQL)
        .bind(article_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;

    let mut article_advance = match current_article_state.as_deref() {
        Some("ai_pending") | Some("ai_done") => {
            sqlite_compute_article_advance(
                &mut tx,
                id,
                article_id,
                keep_decision,
                importance_score,
                min_importance_score,
            )
            .await?
        }
        _ => AiCompleteArticleAdvance::NoChange,
    };

    if let Some(next_state) = article_advance.as_article_state_str() {
        let result = sqlx::query(ADVANCE_ARTICLE_FROM_AI_PHASE_SQL)
            .bind(next_state)
            .bind(now)
            .bind(article_id)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        if result.rows_affected() == 0 {
            article_advance = AiCompleteArticleAdvance::NoChange;
        }
    }

    tx.commit().await.map_err(StorageError::from)?;
    Ok(ReleaseSuccessOutcome {
        released: true,
        article_advance,
    })
}

async fn sqlite_compute_article_advance(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: i64,
    article_id: i64,
    keep_decision: Option<bool>,
    importance_score: Option<i32>,
    min_importance_score: i32,
) -> Result<AiCompleteArticleAdvance, StorageError> {
    match keep_decision {
        Some(true) => {
            let score = importance_score.unwrap_or(0);
            if score >= min_importance_score {
                Ok(AiCompleteArticleAdvance::ReadyForPublish)
            } else {
                Ok(AiCompleteArticleAdvance::AiDone)
            }
        }
        Some(false) => {
            let other_succeeded = sqlx::query_scalar::<_, i32>(OTHER_SUCCEEDED_AI_EXISTS_SQL)
                .bind(article_id)
                .bind(id)
                .fetch_one(&mut **tx)
                .await
                .map_err(StorageError::from)?
                != 0;
            if other_succeeded {
                Ok(AiCompleteArticleAdvance::NoChange)
            } else {
                Ok(AiCompleteArticleAdvance::PublishSkipped)
            }
        }
        None => Ok(AiCompleteArticleAdvance::NoChange),
    }
}

// ── PostgreSQL helper（W11-P3-E-3） ─────────────────────────────

async fn pg_insert_pending(pool: &PgPool, item: &NewAiResult) -> Result<Option<i64>, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_AI_PENDING_SQL)
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| classify_insert_error(error, item))
}

async fn pg_claim_pending(
    pool: &PgPool,
    request: &ClaimRequest,
    category_key: &str,
) -> Result<Vec<ClaimedAiResult>, StorageError> {
    sqlx::query_as::<_, ClaimedAiResult>(CLAIM_AI_PENDING_PG_SQL)
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
        .bind(request.now)
        .bind(request.now)
        .bind(i64::from(request.max_attempts))
        .bind(category_key)
        .bind(i64::from(request.batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

async fn pg_release_success(
    pool: &PgPool,
    id: i64,
    owner: &str,
    outcome: AiSuccessOutcome,
    effective_model_id: &str,
    now: OffsetDateTime,
) -> Result<bool, StorageError> {
    let state = if outcome.keep_decision == Some(false) {
        "filtered"
    } else {
        "succeeded"
    };
    let keep_decision = outcome.keep_decision.map(i32::from);
    let result = sqlx::query(RELEASE_AI_SUCCESS_SQL)
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
        .bind(effective_model_id)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected() == 1)
}

async fn pg_release_ai_failure(
    pool: &PgPool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    now: OffsetDateTime,
    state: &str,
) -> Result<bool, StorageError> {
    let result = sqlx::query(RELEASE_AI_FAILURE_SQL)
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

async fn pg_release_ai_retryable_failure(
    pool: &PgPool,
    id: i64,
    owner: &str,
    error: &str,
    kind: &str,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<ReleaseFailureOutcome, StorageError> {
    let state = sqlx::query_scalar::<_, String>(RELEASE_AI_RETRYABLE_FAILURE_SQL)
        .bind(i64::from(max_attempts))
        .bind(error)
        .bind(kind)
        .bind(now)
        .bind(id)
        .bind(owner)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(ReleaseFailureOutcome {
        released: state.is_some(),
        exhausted: state.as_deref() == Some("permanent_failed"),
    })
}

async fn pg_reclaim_expired_leases(
    pool: &PgPool,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(RECLAIM_AI_LEASES_SQL)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

async fn pg_terminalize_exhausted(
    pool: &PgPool,
    max_attempts: u32,
    now: OffsetDateTime,
) -> Result<u64, StorageError> {
    let result = sqlx::query(TERMINALIZE_EXHAUSTED_AI_SQL)
        .bind(now)
        .bind(i64::from(max_attempts))
        .bind(now)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    Ok(result.rows_affected())
}

async fn pg_insert_pending_and_advance(
    pool: &PgPool,
    item: &NewAiResult,
    min_importance_score: i32,
    now: OffsetDateTime,
) -> Result<InsertPendingOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let inserted_id = sqlx::query_scalar::<_, i64>(INSERT_AI_PENDING_SQL)
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| classify_insert_error(error, item))?;

    let Some(ai_result_id) = inserted_id else {
        let outcome = pg_reconcile_conflict(&mut tx, item, Some(min_importance_score), now).await?;
        tx.commit().await.map_err(StorageError::from)?;
        return Ok(outcome);
    };

    let result = sqlx::query(ADVANCE_ARTICLE_TO_AI_PENDING_SQL)
        .bind(now)
        .bind(item.article_id)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;

    if result.rows_affected() == 0 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(InsertPendingOutcome {
            ai_result_id: None,
            article_advanced: false,
            article_already_advanced: true,
            reconciled: false,
        });
    }

    tx.commit().await.map_err(StorageError::from)?;
    Ok(InsertPendingOutcome {
        ai_result_id: Some(ai_result_id),
        article_advanced: true,
        article_already_advanced: false,
        reconciled: false,
    })
}

/// Conflict branch shared by task_gen and backfill (see the trait docs).
/// `min_importance_score = None` (backfill) leaves finished rows alone.
async fn pg_reconcile_conflict(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    item: &NewAiResult,
    min_importance_score: Option<i32>,
    now: OffsetDateTime,
) -> Result<InsertPendingOutcome, StorageError> {
    let row = sqlx::query_as::<_, IdentityRow>(SELECT_AI_IDENTITY_ROW_PG_SQL)
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(StorageError::from)?;
    let article_state = sqlx::query_scalar::<_, String>(SELECT_ARTICLE_STATE_SQL)
        .bind(item.article_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(StorageError::from)?;
    if article_state.as_deref() != Some("persisted") {
        return Ok(conflict_outcome(article_state.as_deref(), false));
    }
    let next_state = match conflict_target(row, min_importance_score) {
        ConflictTarget::AiPending => Some("ai_pending"),
        ConflictTarget::Finished {
            id,
            keep_decision,
            importance_score,
            min_importance_score,
        } => pg_compute_article_advance(
            tx,
            id,
            item.article_id,
            keep_decision,
            importance_score,
            min_importance_score,
        )
        .await?
        .as_article_state_str(),
        ConflictTarget::Leave => None,
    };
    let Some(next_state) = next_state else {
        return Ok(conflict_outcome(article_state.as_deref(), false));
    };
    let advanced = sqlx::query(ADVANCE_PERSISTED_ARTICLE_SQL)
        .bind(next_state)
        .bind(now)
        .bind(item.article_id)
        .execute(&mut **tx)
        .await
        .map_err(StorageError::from)?
        .rows_affected()
        == 1;
    Ok(conflict_outcome(article_state.as_deref(), advanced))
}

async fn pg_insert_backfill_pending(
    pool: &PgPool,
    item: &NewAiResult,
    now: OffsetDateTime,
) -> Result<InsertPendingOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let inserted_id = sqlx::query_scalar::<_, i64>(INSERT_AI_PENDING_SQL)
        .bind(item.article_id)
        .bind(item.prompt_version)
        .bind(item.output_schema_version)
        .bind(&item.model_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| classify_insert_error(error, item))?;
    let outcome = match inserted_id {
        Some(ai_result_id) => {
            // Unlike task_gen, the row is kept when the article has moved on.
            let advanced = sqlx::query(ADVANCE_ARTICLE_TO_AI_PENDING_SQL)
                .bind(now)
                .bind(item.article_id)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::from)?
                .rows_affected()
                == 1;
            InsertPendingOutcome {
                ai_result_id: Some(ai_result_id),
                article_advanced: advanced,
                article_already_advanced: !advanced,
                reconciled: false,
            }
        }
        None => pg_reconcile_conflict(&mut tx, item, None, now).await?,
    };
    tx.commit().await.map_err(StorageError::from)?;
    Ok(outcome)
}

async fn pg_release_success_and_advance(
    pool: &PgPool,
    id: i64,
    owner: &str,
    outcome: AiSuccessOutcome,
    effective_model_id: &str,
    article_id: i64,
    min_importance_score: i32,
    now: OffsetDateTime,
) -> Result<ReleaseSuccessOutcome, StorageError> {
    let keep_decision = outcome.keep_decision;
    let importance_score = outcome.importance_score;
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let state = if keep_decision == Some(false) {
        "filtered"
    } else {
        "succeeded"
    };
    let keep_decision_i32 = keep_decision.map(i32::from);
    let result = sqlx::query(RELEASE_AI_SUCCESS_SQL)
        .bind(state)
        .bind(outcome.summary)
        .bind(outcome.tags_json)
        .bind(importance_score)
        .bind(keep_decision_i32)
        .bind(outcome.raw_response_artifact_id)
        .bind(outcome.tokens_in)
        .bind(outcome.tokens_out)
        .bind(outcome.cost_micro_usd)
        .bind(outcome.latency_ms)
        .bind(effective_model_id)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;

    if result.rows_affected() == 0 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(ReleaseSuccessOutcome {
            released: false,
            article_advance: AiCompleteArticleAdvance::NoChange,
        });
    }

    let current_article_state = sqlx::query_scalar::<_, String>(SELECT_ARTICLE_STATE_SQL)
        .bind(article_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;

    let mut article_advance = match current_article_state.as_deref() {
        Some("ai_pending") | Some("ai_done") => {
            pg_compute_article_advance(
                &mut tx,
                id,
                article_id,
                keep_decision,
                importance_score,
                min_importance_score,
            )
            .await?
        }
        _ => AiCompleteArticleAdvance::NoChange,
    };

    if let Some(next_state) = article_advance.as_article_state_str() {
        let result = sqlx::query(ADVANCE_ARTICLE_FROM_AI_PHASE_SQL)
            .bind(next_state)
            .bind(now)
            .bind(article_id)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        if result.rows_affected() == 0 {
            article_advance = AiCompleteArticleAdvance::NoChange;
        }
    }

    tx.commit().await.map_err(StorageError::from)?;
    Ok(ReleaseSuccessOutcome {
        released: true,
        article_advance,
    })
}

async fn pg_compute_article_advance(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
    article_id: i64,
    keep_decision: Option<bool>,
    importance_score: Option<i32>,
    min_importance_score: i32,
) -> Result<AiCompleteArticleAdvance, StorageError> {
    match keep_decision {
        Some(true) => {
            let score = importance_score.unwrap_or(0);
            if score >= min_importance_score {
                Ok(AiCompleteArticleAdvance::ReadyForPublish)
            } else {
                Ok(AiCompleteArticleAdvance::AiDone)
            }
        }
        Some(false) => {
            let other_succeeded = sqlx::query_scalar::<_, i32>(OTHER_SUCCEEDED_AI_EXISTS_SQL)
                .bind(article_id)
                .bind(id)
                .fetch_one(&mut **tx)
                .await
                .map_err(StorageError::from)?
                != 0;
            if other_succeeded {
                Ok(AiCompleteArticleAdvance::NoChange)
            } else {
                Ok(AiCompleteArticleAdvance::PublishSkipped)
            }
        }
        None => Ok(AiCompleteArticleAdvance::NoChange),
    }
}
