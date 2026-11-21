//! [`ArticleAiResultRepository`] trait 实装。
//!
//! W11-P3-E-3：每方法按 backend `match &self.pool` 分发到 sqlite_*/pg_* 私有
//! helper；SQL const 集中在 [`super::article_ai_result_sql`]；claim 路径在 PG 加
//! `FOR UPDATE SKIP LOCKED`（§6.4 契约），故两侧 SQL 分叉。

use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, ReleaseFailureOutcome, StorageError, StoragePool, classify_db_error};

use super::{sqlite_instant_cutoff, sqlite_stored_deadline, sqlite_window_bounds};

use super::article_ai_result::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepo, ArticleAiResultRepository,
    ClaimedAiResult, FreshWindow, InsertPendingOutcome, NewAiResult, ReleaseSuccessOutcome,
    ShareMark, ShareSide, ShareStage,
};
use super::article_ai_result_sql::{
    ADVANCE_ARTICLE_FROM_AI_PHASE_SQL, ADVANCE_ARTICLE_TO_AI_PENDING_SQL,
    ADVANCE_PERSISTED_ARTICLE_SQL, BEST_OTHER_KEPT_SCORE_SQL, CLAIM_AI_BY_ID_SQL,
    CLAIM_AI_PENDING_PG_SQL, CLAIM_AI_PENDING_SQLITE_SQL, INSERT_AI_PENDING_SQL,
    OTHER_SUCCEEDED_AI_EXISTS_SQL, RECLAIM_AI_LEASES_SQL, RECLAIM_AI_LEASES_SQLITE_SQL,
    RELEASE_AI_FAILURE_SQL, RELEASE_AI_RETRYABLE_FAILURE_SQL, RELEASE_AI_SUCCESS_SQL,
    SELECT_AI_IDENTITY_ROW_PG_SQL, SELECT_AI_IDENTITY_ROW_SQLITE_SQL, SELECT_ARTICLE_STATE_SQL,
    SELECT_BACKLOG_CLAIM_CANDIDATES_PG_SQL, SELECT_BACKLOG_CLAIM_CANDIDATES_SQLITE_SQL,
    SELECT_FRESH_CLAIM_CANDIDATES_PG_SQL, SELECT_FRESH_CLAIM_CANDIDATES_SQLITE_SQL,
    SELECT_SHARE_SIDE_SQL, TERMINALIZE_EXHAUSTED_AI_SQL, TERMINALIZE_EXHAUSTED_AI_SQLITE_SQL,
    UPSERT_SHARE_SIDE_SQL,
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

/// The post-AI state a kept result with score `best` implies (`None`: no
/// kept result, leave the article).
fn advance_from_kept_score(
    best: Option<i32>,
    min_importance_score: i32,
) -> AiCompleteArticleAdvance {
    match best {
        Some(score) if score >= min_importance_score => AiCompleteArticleAdvance::ReadyForPublish,
        Some(_) => AiCompleteArticleAdvance::AiDone,
        None => AiCompleteArticleAdvance::NoChange,
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

fn parse_share_side(value: &str) -> Result<ShareSide, StorageError> {
    ShareSide::parse(value).ok_or_else(|| {
        StorageError::Corruption(format!("ai_fresh_share_state.last_side = {value:?}"))
    })
}

/// Records `mark` inside the caller's SQLite transaction (R2-B1).
async fn sqlite_upsert_share_side(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    mark: &ShareMark<'_>,
    now: OffsetDateTime,
) -> Result<(), StorageError> {
    sqlx::query(UPSERT_SHARE_SIDE_SQL)
        .bind(mark.category_key)
        .bind(mark.stage.as_str())
        .bind(mark.side.as_str())
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(StorageError::from)?;
    Ok(())
}

/// Records `mark` inside the caller's PostgreSQL transaction (R2-B1).
async fn pg_upsert_share_side(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mark: &ShareMark<'_>,
    now: OffsetDateTime,
) -> Result<(), StorageError> {
    sqlx::query(UPSERT_SHARE_SIDE_SQL)
        .bind(mark.category_key)
        .bind(mark.stage.as_str())
        .bind(mark.side.as_str())
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(StorageError::from)?;
    Ok(())
}

/// Quota bookkeeping of one shared claim batch (C9, plan/03 §5.2).
///
/// Steps: ① the leading side gets `ceil(n/2)`; ② the other side gets what
/// ① left; ③ if ① was not short and slots remain, the leading side takes
/// them. A step whose limit is 0 issues no query (R2-B2).
/// Invariant: the sum of recorded rows never exceeds `n`.
struct SharedClaimPlan {
    n: u32,
    fresh_first: bool,
    step: usize,
    current: usize,
    got: [u32; 3],
}

impl SharedClaimPlan {
    fn new(n: u32, fresh_first: bool) -> Self {
        Self {
            n,
            fresh_first,
            step: 0,
            current: 0,
            got: [0; 3],
        }
    }

    /// The next `(fresh side?, limit)` query, skipping zero-limit steps.
    fn next_query(&mut self) -> Option<(bool, u32)> {
        while self.step < 3 {
            let step = self.step;
            self.step += 1;
            let lead_quota = self.n.div_ceil(2);
            let query = match step {
                0 => (self.fresh_first, lead_quota),
                1 => (!self.fresh_first, self.n - self.got[0]),
                _ => {
                    // The sides are disjoint (R2-B1): a short leading side is
                    // exhausted, a full one may still hold rows.
                    if self.got[0] < lead_quota {
                        continue;
                    }
                    (self.fresh_first, self.n - self.got[0] - self.got[1])
                }
            };
            if query.1 > 0 {
                self.current = step;
                return Some(query);
            }
        }
        None
    }

    fn record(&mut self, rows: usize) {
        self.got[self.current] = rows as u32;
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

    async fn claim_pending_shared(
        &self,
        request: &ClaimRequest,
        category_key: &str,
        published_since: OffsetDateTime,
    ) -> Result<Vec<ClaimedAiResult>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_claim_pending_shared(p, request, category_key, published_since).await
            }
            StoragePool::Postgres(p) => {
                pg_claim_pending_shared(p, request, category_key, published_since).await
            }
        }
    }

    async fn last_served_side(
        &self,
        category_key: &str,
        stage: ShareStage,
    ) -> Result<Option<ShareSide>, StorageError> {
        let side = match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlx::query_scalar::<_, String>(SELECT_SHARE_SIDE_SQL)
                    .bind(category_key)
                    .bind(stage.as_str())
                    .fetch_optional(p)
                    .await
            }
            StoragePool::Postgres(p) => {
                sqlx::query_scalar::<_, String>(SELECT_SHARE_SIDE_SQL)
                    .bind(category_key)
                    .bind(stage.as_str())
                    .fetch_optional(p)
                    .await
            }
        }
        .map_err(StorageError::from)?;
        side.map(|value| parse_share_side(&value)).transpose()
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
                sqlite_insert_pending_and_advance(p, item, min_importance_score, now, None).await
            }
            StoragePool::Postgres(p) => {
                pg_insert_pending_and_advance(p, item, min_importance_score, now, None).await
            }
        }
    }

    async fn insert_pending_advance_and_mark(
        &self,
        item: &NewAiResult,
        min_importance_score: i32,
        now: OffsetDateTime,
        mark: &ShareMark<'_>,
    ) -> Result<InsertPendingOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_insert_pending_and_advance(p, item, min_importance_score, now, Some(mark))
                    .await
            }
            StoragePool::Postgres(p) => {
                pg_insert_pending_and_advance(p, item, min_importance_score, now, Some(mark)).await
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

/// R2-B2: the whole batch in one `BEGIN IMMEDIATE` transaction (the write
/// lock is taken up front, so the reads inside cannot be invalidated by a
/// concurrent writer before the claim UPDATEs).
async fn sqlite_claim_pending_shared(
    pool: &SqlitePool,
    request: &ClaimRequest,
    category_key: &str,
    published_since: OffsetDateTime,
) -> Result<Vec<ClaimedAiResult>, StorageError> {
    let n = request.batch_size;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(StorageError::from)?;
    // One window per batch, ending at the claim's own `now` (R2-S1).
    let (since, until) = sqlite_window_bounds(FreshWindow {
        since: published_since,
        until: request.now,
    });
    let fresh_first = if n == 1 {
        let last = sqlx::query_scalar::<_, String>(SELECT_SHARE_SIDE_SQL)
            .bind(category_key)
            .bind(ShareStage::Claim.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        last.map(|value| parse_share_side(&value)).transpose()? != Some(ShareSide::Fresh)
    } else {
        true
    };
    let mut plan = SharedClaimPlan::new(n, fresh_first);
    let mut claimed = Vec::new();
    let mut served = None;
    while let Some((fresh, limit)) = plan.next_query() {
        let sql = if fresh {
            SELECT_FRESH_CLAIM_CANDIDATES_SQLITE_SQL
        } else {
            SELECT_BACKLOG_CLAIM_CANDIDATES_SQLITE_SQL
        };
        let candidates = sqlx::query_scalar::<_, i64>(sql)
            .bind(sqlite_instant_cutoff(request.now))
            .bind(i64::from(request.max_attempts))
            .bind(category_key)
            .bind(i64::from(limit))
            .bind(since)
            .bind(until)
            .fetch_all(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        // Claimed one by one so the rows keep the candidate order (R2-S2).
        let mut rows = Vec::with_capacity(candidates.len());
        for id in candidates {
            let row = sqlx::query_as::<_, ClaimedAiResult>(CLAIM_AI_BY_ID_SQL)
                .bind(&request.owner)
                .bind(sqlite_stored_deadline(request.lease_expires_at))
                .bind(request.now)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(StorageError::from)?;
            rows.extend(row);
        }
        plan.record(rows.len());
        if !rows.is_empty() {
            served = Some(side_of(fresh));
        }
        claimed.extend(rows);
    }
    if let (1, Some(side)) = (n, served) {
        let mark = ShareMark {
            category_key,
            stage: ShareStage::Claim,
            side,
        };
        sqlite_upsert_share_side(&mut tx, &mark, request.now).await?;
    }
    tx.commit().await.map_err(StorageError::from)?;
    Ok(claimed)
}

fn side_of(fresh: bool) -> ShareSide {
    if fresh {
        ShareSide::Fresh
    } else {
        ShareSide::Backlog
    }
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
    mark: Option<&ShareMark<'_>>,
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
        if let (true, Some(mark)) = (outcome.article_advanced, mark) {
            sqlite_upsert_share_side(&mut tx, mark, now).await?;
        }
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

    if let Some(mark) = mark {
        sqlite_upsert_share_side(&mut tx, mark, now).await?;
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
        } => {
            let advance = sqlite_compute_article_advance(
                tx,
                id,
                item.article_id,
                keep_decision,
                importance_score,
                min_importance_score,
            )
            .await?;
            // R2-S3: this identity filtered the article but another one
            // succeeded. A success release leaves the article where that
            // success put it (`NoChange`); a still-`persisted` article was
            // never put anywhere, so derive the state from the kept result.
            let advance =
                if advance == AiCompleteArticleAdvance::NoChange && keep_decision == Some(false) {
                    let best = sqlx::query_scalar::<_, Option<i32>>(BEST_OTHER_KEPT_SCORE_SQL)
                        .bind(item.article_id)
                        .bind(id)
                        .fetch_one(&mut **tx)
                        .await
                        .map_err(StorageError::from)?;
                    advance_from_kept_score(best, min_importance_score)
                } else {
                    advance
                };
            advance.as_article_state_str()
        }
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

/// R2-B2: the whole batch in one transaction; the `FOR UPDATE OF aar SKIP
/// LOCKED` row locks of every side are held until the single commit.
async fn pg_claim_pending_shared(
    pool: &PgPool,
    request: &ClaimRequest,
    category_key: &str,
    published_since: OffsetDateTime,
) -> Result<Vec<ClaimedAiResult>, StorageError> {
    let n = request.batch_size;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let fresh_first = if n == 1 {
        // Concurrent batch-size-1 workers of one category may both read the
        // same side; the upsert below keeps the last writer (best effort,
        // each still serves a real task).
        let last = sqlx::query_scalar::<_, String>(SELECT_SHARE_SIDE_SQL)
            .bind(category_key)
            .bind(ShareStage::Claim.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        last.map(|value| parse_share_side(&value)).transpose()? != Some(ShareSide::Fresh)
    } else {
        true
    };
    let mut plan = SharedClaimPlan::new(n, fresh_first);
    let mut claimed = Vec::new();
    let mut served = None;
    while let Some((fresh, limit)) = plan.next_query() {
        let sql = if fresh {
            SELECT_FRESH_CLAIM_CANDIDATES_PG_SQL
        } else {
            SELECT_BACKLOG_CLAIM_CANDIDATES_PG_SQL
        };
        let candidates = sqlx::query_scalar::<_, i64>(sql)
            .bind(request.now)
            .bind(i64::from(request.max_attempts))
            .bind(category_key)
            .bind(i64::from(limit))
            .bind(published_since)
            .bind(request.now)
            .fetch_all(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        // Claimed one by one so the rows keep the candidate order (R2-S2).
        let mut rows = Vec::with_capacity(candidates.len());
        for id in candidates {
            let row = sqlx::query_as::<_, ClaimedAiResult>(CLAIM_AI_BY_ID_SQL)
                .bind(&request.owner)
                .bind(request.lease_expires_at)
                .bind(request.now)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(StorageError::from)?;
            rows.extend(row);
        }
        plan.record(rows.len());
        if !rows.is_empty() {
            served = Some(side_of(fresh));
        }
        claimed.extend(rows);
    }
    if let (1, Some(side)) = (n, served) {
        let mark = ShareMark {
            category_key,
            stage: ShareStage::Claim,
            side,
        };
        pg_upsert_share_side(&mut tx, &mark, request.now).await?;
    }
    tx.commit().await.map_err(StorageError::from)?;
    Ok(claimed)
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
    mark: Option<&ShareMark<'_>>,
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
        if let (true, Some(mark)) = (outcome.article_advanced, mark) {
            pg_upsert_share_side(&mut tx, mark, now).await?;
        }
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

    if let Some(mark) = mark {
        pg_upsert_share_side(&mut tx, mark, now).await?;
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
        } => {
            let advance = pg_compute_article_advance(
                tx,
                id,
                item.article_id,
                keep_decision,
                importance_score,
                min_importance_score,
            )
            .await?;
            // R2-S3: this identity filtered the article but another one
            // succeeded. A success release leaves the article where that
            // success put it (`NoChange`); a still-`persisted` article was
            // never put anywhere, so derive the state from the kept result.
            let advance =
                if advance == AiCompleteArticleAdvance::NoChange && keep_decision == Some(false) {
                    let best = sqlx::query_scalar::<_, Option<i32>>(BEST_OTHER_KEPT_SCORE_SQL)
                        .bind(item.article_id)
                        .bind(id)
                        .fetch_one(&mut **tx)
                        .await
                        .map_err(StorageError::from)?;
                    advance_from_kept_score(best, min_importance_score)
                } else {
                    advance
                };
            advance.as_article_state_str()
        }
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
#[cfg(test)]
mod tests {
    use super::SharedClaimPlan;

    /// Runs the plan against sides holding `fresh` / `other` rows.
    fn run(n: u32, fresh_first: bool, fresh: u32, other: u32) -> Vec<(bool, u32)> {
        let mut plan = SharedClaimPlan::new(n, fresh_first);
        let (mut fresh_left, mut other_left) = (fresh, other);
        let mut queries = Vec::new();
        while let Some((is_fresh, limit)) = plan.next_query() {
            queries.push((is_fresh, limit));
            let left = if is_fresh {
                &mut fresh_left
            } else {
                &mut other_left
            };
            let got = limit.min(*left);
            *left -= got;
            plan.record(got as usize);
        }
        queries
    }

    #[test]
    fn a_full_leading_side_issues_no_zero_limit_query() {
        // n = 1: the leading side fills the batch; no LIMIT 0 query follows.
        assert_eq!(run(1, true, 5, 5), [(true, 1)]);
        assert_eq!(run(1, false, 5, 5), [(false, 1)]);
        // n = 2, both sides full: exactly one query per side.
        assert_eq!(run(2, true, 5, 5), [(true, 1), (false, 1)]);
    }

    #[test]
    fn a_short_side_passes_its_share_on() {
        assert_eq!(run(4, true, 0, 9), [(true, 2), (false, 4)]);
        assert_eq!(run(4, false, 9, 1), [(false, 2), (true, 3)]);
        assert_eq!(run(1, true, 0, 3), [(true, 1), (false, 1)]);
    }
}
