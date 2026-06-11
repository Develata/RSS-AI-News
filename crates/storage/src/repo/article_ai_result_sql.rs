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
      AND (aar.lease_expires_at IS NULL OR aar.lease_expires_at < $5)
      AND aar.attempt_count < $6
      AND fs.category_key = $7
    ORDER BY aar.id ASC
    LIMIT $8
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id
"#;

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
    FOR UPDATE SKIP LOCKED
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id
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

pub(super) const ADVANCE_ARTICLE_TO_AI_PENDING_SQL: &str = r#"
UPDATE articles
SET state = 'ai_pending', updated_at = $1
WHERE id = $2 AND state = 'persisted'
"#;

pub(super) const SELECT_ARTICLE_STATE_SQL: &str = "SELECT state FROM articles WHERE id = $1";

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
