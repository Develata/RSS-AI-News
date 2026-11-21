//! article_ai_results 共享 SQL 字符串。
//!
//! W11-P3-E-3：除两条 claim 路径（PG 加 `FOR UPDATE SKIP LOCKED`，§6.4 契约）
//! 外，所有 const 跨方言完全等价。const 由 [`super::article_ai_result_impl`] 的
//! sqlite_*/pg_* helper 共享。

pub(super) const INSERT_AI_PENDING_SQL: &str = r#"
INSERT INTO article_ai_results (
    article_id, prompt_version, output_schema_version, model_id, state
)
VALUES ($1, $2, $3, $4, 'pending')
ON CONFLICT(article_id, prompt_version, output_schema_version, model_id) DO NOTHING
RETURNING id
"#;

/// SQLite claim. SQLite stores times as RFC3339 TEXT with a variable-width
/// fraction, so text order is not instant order within a second; the lease
/// predicate compares `julianday()` instants. `$2` is written through
/// `sqlite_stored_deadline` (UTC, rounded up to the ms) and `$5` is
/// `sqlite_instant_cutoff(now)` (rounded down), so the comparison is exact
/// and never early. Text unparseable by `julianday()` (NULL) falls back to the
/// old text comparison via `COALESCE` (same rule as `feed_entries`).
pub(super) const CLAIM_AI_PENDING_SQLITE_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, $3),
    updated_at = $4
WHERE id IN (
    SELECT aar.id
    FROM article_ai_results aar
    JOIN articles a ON a.id = aar.article_id
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE aar.state = 'pending'
      AND (aar.lease_expires_at IS NULL
           OR COALESCE(julianday(aar.lease_expires_at) < julianday($5),
                       aar.lease_expires_at < $5))
      AND aar.attempt_count < $6
      AND fs.category_key = $7
    ORDER BY aar.id ASC
    LIMIT $8
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id
"#;

/// PG claim. `FOR UPDATE OF aar`: only the AI rows are locked / skipped. A
/// bare `FOR UPDATE` would lock every joined table, so a source or article
/// row locked by another transaction would hide claimable AI rows and the
/// worker would read the empty batch as an exhausted queue.
pub(super) const CLAIM_AI_PENDING_PG_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, $3),
    updated_at = $4
WHERE id IN (
    SELECT aar.id
    FROM article_ai_results aar
    JOIN articles a ON a.id = aar.article_id
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE aar.state = 'pending'
      AND (aar.lease_expires_at IS NULL OR aar.lease_expires_at < $5)
      AND aar.attempt_count < $6
      AND fs.category_key = $7
    ORDER BY aar.id ASC
    LIMIT $8
    FOR UPDATE OF aar SKIP LOCKED
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id
"#;

// ── C9: publish-window ("fresh") share ────────────────────────────────
// "Fresh" = the article would be eligible if publish froze now:
// `published_since <= COALESCE(fe.published_at, fe.discovered_at) <= now`
// (the freeze's bounds, `flows/publish/freeze.rs`; R2-S1: a future-dated
// article is not fresh, today's freeze cannot select it). SQLite compares and
// orders that source time by `julianday()` instant with a text fallback; `$9`
// / `$10` are `sqlite_window_bounds` (rounded inward to whole ms). The
// backlog claim is the exact complement (R2-B1), so the two sides never
// compete for the same task.

/// SQLite fresh claim: pending tasks of in-window articles, oldest source
/// time first (they leave the window first), ties by task id. Lease,
/// attempt and category predicates are those of [`CLAIM_AI_PENDING_SQLITE_SQL`].
pub(super) const CLAIM_AI_PENDING_FRESH_SQLITE_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, $3),
    updated_at = $4
WHERE id IN (
    SELECT aar.id
    FROM article_ai_results aar
    JOIN articles a ON a.id = aar.article_id
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE aar.state = 'pending'
      AND (aar.lease_expires_at IS NULL
           OR COALESCE(julianday(aar.lease_expires_at) < julianday($5),
                       aar.lease_expires_at < $5))
      AND aar.attempt_count < $6
      AND fs.category_key = $7
      AND COALESCE(julianday(COALESCE(fe.published_at, fe.discovered_at)) >= julianday($9),
                   COALESCE(fe.published_at, fe.discovered_at) >= $9)
      AND COALESCE(julianday(COALESCE(fe.published_at, fe.discovered_at)) <= julianday($10),
                   COALESCE(fe.published_at, fe.discovered_at) <= $10)
    ORDER BY julianday(COALESCE(fe.published_at, fe.discovered_at)) ASC,
             COALESCE(fe.published_at, fe.discovered_at) ASC,
             aar.id ASC
    LIMIT $8
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id
"#;

/// PG fresh claim; `FOR UPDATE OF aar` as in [`CLAIM_AI_PENDING_PG_SQL`].
pub(super) const CLAIM_AI_PENDING_FRESH_PG_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, $3),
    updated_at = $4
WHERE id IN (
    SELECT aar.id
    FROM article_ai_results aar
    JOIN articles a ON a.id = aar.article_id
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE aar.state = 'pending'
      AND (aar.lease_expires_at IS NULL OR aar.lease_expires_at < $5)
      AND aar.attempt_count < $6
      AND fs.category_key = $7
      AND COALESCE(fe.published_at, fe.discovered_at) >= $9
      AND COALESCE(fe.published_at, fe.discovered_at) <= $10
    ORDER BY COALESCE(fe.published_at, fe.discovered_at) ASC, aar.id ASC
    LIMIT $8
    FOR UPDATE OF aar SKIP LOCKED
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id
"#;

/// SQLite backlog claim (R2-B1): pending tasks of articles **outside** the
/// window — the complement of [`CLAIM_AI_PENDING_FRESH_SQLITE_SQL`] — by task
/// id. Predicates and binds as in the fresh claim.
pub(super) const CLAIM_AI_PENDING_BACKLOG_SQLITE_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, $3),
    updated_at = $4
WHERE id IN (
    SELECT aar.id
    FROM article_ai_results aar
    JOIN articles a ON a.id = aar.article_id
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE aar.state = 'pending'
      AND (aar.lease_expires_at IS NULL
           OR COALESCE(julianday(aar.lease_expires_at) < julianday($5),
                       aar.lease_expires_at < $5))
      AND aar.attempt_count < $6
      AND fs.category_key = $7
      AND NOT (COALESCE(julianday(COALESCE(fe.published_at, fe.discovered_at)) >= julianday($9),
                        COALESCE(fe.published_at, fe.discovered_at) >= $9)
               AND COALESCE(julianday(COALESCE(fe.published_at, fe.discovered_at)) <= julianday($10),
                            COALESCE(fe.published_at, fe.discovered_at) <= $10))
    ORDER BY aar.id ASC
    LIMIT $8
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id
"#;

/// PG backlog claim; `FOR UPDATE OF aar` as in [`CLAIM_AI_PENDING_PG_SQL`].
pub(super) const CLAIM_AI_PENDING_BACKLOG_PG_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, $3),
    updated_at = $4
WHERE id IN (
    SELECT aar.id
    FROM article_ai_results aar
    JOIN articles a ON a.id = aar.article_id
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE aar.state = 'pending'
      AND (aar.lease_expires_at IS NULL OR aar.lease_expires_at < $5)
      AND aar.attempt_count < $6
      AND fs.category_key = $7
      AND NOT (COALESCE(fe.published_at, fe.discovered_at) >= $9
               AND COALESCE(fe.published_at, fe.discovered_at) <= $10)
    ORDER BY aar.id ASC
    LIMIT $8
    FOR UPDATE OF aar SKIP LOCKED
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id
"#;

/// C9 / R2-B1 batch-size-1 alternation state (migration 0009).
pub(super) const SELECT_SHARE_SIDE_SQL: &str =
    "SELECT last_side FROM ai_fresh_share_state WHERE category_key = $1 AND stage = $2";

pub(super) const UPSERT_SHARE_SIDE_SQL: &str = r#"
INSERT INTO ai_fresh_share_state (category_key, stage, last_side, updated_at)
VALUES ($1, $2, $3, $4)
ON CONFLICT (category_key, stage)
DO UPDATE SET last_side = excluded.last_side, updated_at = excluded.updated_at
"#;

pub(super) const RELEASE_AI_SUCCESS_SQL: &str = r#"
UPDATE article_ai_results
SET state = $1, summary = $2, tags_json = $3, importance_score = $4,
    keep_decision = $5, raw_response_artifact_id = $6, tokens_in = $7,
    tokens_out = $8, cost_micro_usd = $9, latency_ms = $10,
    effective_model_id = $11,
    lease_owner = NULL, lease_expires_at = NULL,
    last_error = NULL, last_error_kind = NULL,
    completed_at = $12, updated_at = $13
WHERE id = $14 AND lease_owner = $15
"#;

pub(super) const RELEASE_AI_FAILURE_SQL: &str = r#"
UPDATE article_ai_results
SET state = $1, lease_owner = NULL, lease_expires_at = NULL,
    last_error = $2, last_error_kind = $3, updated_at = $4
WHERE id = $5 AND lease_owner = $6
"#;

/// W15 §3 折叠：retryable 失败时按预算决定回队 / 转终态，规则收口在 SQL，
/// 任何 flow 都不可能造出"耗尽仍回 pending"的卡死行。`RETURNING state`
/// 供调用方判定走向。claim 过滤 `attempt_count < max` 发生在自增前，
/// 故 release 时 `attempt_count >= max` 当且仅当本次是预算内最后一次尝试。
pub(super) const RELEASE_AI_RETRYABLE_FAILURE_SQL: &str = r#"
UPDATE article_ai_results
SET state = CASE WHEN attempt_count >= $1 THEN 'permanent_failed' ELSE 'pending' END,
    lease_owner = NULL, lease_expires_at = NULL,
    last_error = $2, last_error_kind = $3, updated_at = $4
WHERE id = $5 AND lease_owner = $6
RETURNING state
"#;

pub(super) const RECLAIM_AI_LEASES_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'pending',
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE lease_expires_at IS NOT NULL
  AND lease_expires_at < $2
  AND state = 'running'
"#;

/// SQLite variant: instant comparison (see [`CLAIM_AI_PENDING_SQLITE_SQL`]);
/// `$2` is `sqlite_instant_cutoff(now)`.
pub(super) const RECLAIM_AI_LEASES_SQLITE_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'pending',
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE lease_expires_at IS NOT NULL
  AND COALESCE(julianday(lease_expires_at) < julianday($2), lease_expires_at < $2)
  AND state = 'running'
"#;

/// W15 §4 sweep：预算耗尽且 claim 永远不会再领取的 pending 行 → 终态。
/// COALESCE 保留行上既有真实错误（retryable release 已写过），仅对从未留过
/// 错误的行落兜底文案。
pub(super) const TERMINALIZE_EXHAUSTED_AI_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'permanent_failed',
    last_error = COALESCE(last_error, 'retry budget exhausted'),
    last_error_kind = COALESCE(last_error_kind, 'retry_budget_exhausted'),
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE state = 'pending'
  AND attempt_count >= $2
  AND (lease_expires_at IS NULL OR lease_expires_at < $3)
"#;

/// SQLite variant of [`TERMINALIZE_EXHAUSTED_AI_SQL`]: instant comparison;
/// `$3` is `sqlite_instant_cutoff(now)`.
pub(super) const TERMINALIZE_EXHAUSTED_AI_SQLITE_SQL: &str = r#"
UPDATE article_ai_results
SET state = 'permanent_failed',
    last_error = COALESCE(last_error, 'retry budget exhausted'),
    last_error_kind = COALESCE(last_error_kind, 'retry_budget_exhausted'),
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE state = 'pending'
  AND attempt_count >= $2
  AND (lease_expires_at IS NULL
       OR COALESCE(julianday(lease_expires_at) < julianday($3), lease_expires_at < $3))
"#;

pub(super) const ADVANCE_ARTICLE_TO_AI_PENDING_SQL: &str = r#"
UPDATE articles
SET state = 'ai_pending', updated_at = $1
WHERE id = $2 AND state = 'persisted'
"#;

pub(super) const SELECT_ARTICLE_STATE_SQL: &str = "SELECT state FROM articles WHERE id = $1";

/// The existing row of one AI identity (task_gen / backfill conflict branch).
pub(super) const SELECT_AI_IDENTITY_ROW_SQLITE_SQL: &str = r#"
SELECT id, state, keep_decision, importance_score
FROM article_ai_results
WHERE article_id = $1 AND prompt_version = $2 AND output_schema_version = $3 AND model_id = $4
"#;

/// PG: lock the existing row so a concurrent success release (which updates
/// this row first, then reads the article) serialises with the reconcile.
pub(super) const SELECT_AI_IDENTITY_ROW_PG_SQL: &str = r#"
SELECT id, state, keep_decision, importance_score
FROM article_ai_results
WHERE article_id = $1 AND prompt_version = $2 AND output_schema_version = $3 AND model_id = $4
FOR UPDATE
"#;

pub(super) const ADVANCE_PERSISTED_ARTICLE_SQL: &str = r#"
UPDATE articles
SET state = $1, updated_at = $2
WHERE id = $3 AND state = 'persisted'
"#;

pub(super) const ADVANCE_ARTICLE_FROM_AI_PHASE_SQL: &str = r#"
UPDATE articles
SET state = $1, updated_at = $2
WHERE id = $3 AND state IN ('ai_pending', 'ai_done')
"#;

pub(super) const OTHER_SUCCEEDED_AI_EXISTS_SQL: &str = r#"
SELECT CASE WHEN EXISTS (
    SELECT 1
    FROM article_ai_results
    WHERE article_id = $1 AND state = 'succeeded' AND id != $2
) THEN 1 ELSE 0 END
"#;
