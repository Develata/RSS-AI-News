//! SQL for [`super::decision_result`]. Placeholders are `$N` on both
//! dialects; statements differ only where SQLite and PostgreSQL do (time
//! comparison of `articles.created_at`, `FOR UPDATE SKIP LOCKED`).

pub(super) const UPSERT_POLICY_SQL: &str = r#"
INSERT INTO decision_policies (policy_version, canonical_json, created_at)
VALUES ($1, $2, $3)
ON CONFLICT (policy_version) DO NOTHING
"#;

/// `$8` is `sqlite_utc_seconds(cutoff)`; `created_at` is normalised to the
/// same whole-second UTC text first (see `repo::sqlite_utc_seconds`).
pub(super) const GENERATE_PENDING_SQLITE_SQL: &str = r#"
INSERT INTO decision_results (
    article_id, category_key, policy_version, execution_key, provider, requested_model,
    laya_max_len, state, created_at, updated_at
)
SELECT a.id, $1, $2, $3, $4, $5, $6, 'pending', $7, $7
FROM articles a
JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
JOIN feed_sources fs ON fs.id = fe.source_id
WHERE fs.category_key = $1
  AND a.state <> 'retired'
  AND replace(substr(a.created_at, 1, 19), 'T', ' ') >= $8
  AND NOT EXISTS (
      SELECT 1 FROM decision_results d
      WHERE d.article_id = a.id AND d.policy_version = $2 AND d.execution_key = $3
  )
ORDER BY a.id ASC
LIMIT $9
ON CONFLICT (article_id, policy_version, execution_key) DO NOTHING
"#;

/// `$8` is the whole-second cutoff as TIMESTAMPTZ.
pub(super) const GENERATE_PENDING_PG_SQL: &str = r#"
INSERT INTO decision_results (
    article_id, category_key, policy_version, execution_key, provider, requested_model,
    laya_max_len, state, created_at, updated_at
)
SELECT a.id, $1, $2, $3, $4, $5, $6, 'pending', $7, $7
FROM articles a
JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
JOIN feed_sources fs ON fs.id = fe.source_id
WHERE fs.category_key = $1
  AND a.state <> 'retired'
  AND a.created_at >= $8
  AND NOT EXISTS (
      SELECT 1 FROM decision_results d
      WHERE d.article_id = a.id AND d.policy_version = $2 AND d.execution_key = $3
  )
ORDER BY a.id ASC
LIMIT $9
ON CONFLICT (article_id, policy_version, execution_key) DO NOTHING
"#;

pub(super) const CLAIM_SQLITE_SQL: &str = r#"
UPDATE decision_results
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, $3),
    updated_at = $3
WHERE id IN (
    SELECT id FROM decision_results
    WHERE category_key = $4 AND policy_version = $5 AND execution_key = $6
      AND state = 'pending'
      AND (lease_expires_at IS NULL OR lease_expires_at < $3)
      AND attempt_count < $7
    ORDER BY id ASC
    LIMIT $8
)
RETURNING id, article_id, policy_version, execution_key, attempt_count
"#;

pub(super) const CLAIM_PG_SQL: &str = r#"
UPDATE decision_results
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, $3),
    updated_at = $3
WHERE id IN (
    SELECT id FROM decision_results
    WHERE category_key = $4 AND policy_version = $5 AND execution_key = $6
      AND state = 'pending'
      AND (lease_expires_at IS NULL OR lease_expires_at < $3)
      AND attempt_count < $7
    ORDER BY id ASC
    LIMIT $8
    FOR UPDATE SKIP LOCKED
)
RETURNING id, article_id, policy_version, execution_key, attempt_count
"#;

pub(super) const RELEASE_SUCCESS_SQL: &str = r#"
UPDATE decision_results
SET state = 'succeeded',
    lease_owner = NULL,
    lease_expires_at = NULL,
    answers_json = $1,
    reported_model = $2,
    routing_model = $3,
    endpoint_origin = $4,
    state_sha256 = $5,
    body_chars_sent = $6,
    body_truncated = $7,
    provider_truncated = $8,
    state_tokens_dropped = $9,
    input_tokens = $10,
    latency_ms = $11,
    raw_response = $12,
    raw_response_expires_at = $13,
    raw_response_bytes = $14,
    raw_response_sha256 = $15,
    raw_response_withheld = $16,
    last_error_kind = NULL,
    last_error = NULL,
    completed_at = $17,
    updated_at = $17
WHERE id = $18 AND lease_owner = $19
"#;

/// `$1` mode: 0 = charge, 1 = permanent, 2 = refund. Every SET expression
/// reads the row as it was before the update, so `attempt_count` in the CASE
/// is the count after this claim's increment: `>= max` means this was the
/// last budgeted attempt. A refund undoes the claim's increment.
pub(super) const RELEASE_FAILURE_SQL: &str = r#"
UPDATE decision_results
SET state = CASE
        WHEN $1 = 2 THEN 'pending'
        WHEN $1 = 1 OR attempt_count >= $2 THEN 'failed'
        ELSE 'pending'
    END,
    attempt_count = CASE WHEN $1 = 2 THEN attempt_count - 1 ELSE attempt_count END,
    completed_at = CASE
        WHEN $1 <> 2 AND ($1 = 1 OR attempt_count >= $2) THEN $3
        ELSE completed_at
    END,
    lease_owner = NULL,
    lease_expires_at = NULL,
    last_error_kind = $4,
    last_error = $5,
    endpoint_origin = $6,
    state_sha256 = $7,
    body_chars_sent = $8,
    body_truncated = $9,
    latency_ms = $10,
    raw_response = $11,
    raw_response_expires_at = $12,
    raw_response_bytes = $13,
    raw_response_sha256 = $14,
    raw_response_withheld = $15,
    updated_at = $3
WHERE id = $16 AND lease_owner = $17
RETURNING state
"#;

pub(super) const RECLAIM_EXPIRED_SQL: &str = r#"
UPDATE decision_results
SET state = 'pending', lease_owner = NULL, lease_expires_at = NULL, updated_at = $1
WHERE category_key = $2 AND policy_version = $3 AND execution_key = $4
  AND state = 'running'
  AND lease_expires_at < $1
"#;

pub(super) const TERMINALIZE_EXHAUSTED_SQL: &str = r#"
UPDATE decision_results
SET state = 'failed',
    last_error_kind = 'retry_budget_exhausted',
    last_error = COALESCE(last_error, 'retry budget exhausted'),
    completed_at = $1,
    updated_at = $1
WHERE category_key = $2 AND policy_version = $3 AND execution_key = $4
  AND state = 'pending'
  AND attempt_count >= $5
  AND (lease_expires_at IS NULL OR lease_expires_at < $1)
"#;

/// Both columns are cleared, so a purged row is never selected again.
pub(super) const PURGE_EXPIRED_RAW_SQLITE_SQL: &str = r#"
UPDATE decision_results
SET raw_response = NULL, raw_response_expires_at = NULL
WHERE id IN (
    SELECT id FROM decision_results
    WHERE raw_response IS NOT NULL
      AND raw_response_expires_at IS NOT NULL
      AND raw_response_expires_at < $1
    ORDER BY raw_response_expires_at ASC
    LIMIT $2
)
"#;

pub(super) const PURGE_EXPIRED_RAW_PG_SQL: &str = r#"
UPDATE decision_results
SET raw_response = NULL, raw_response_expires_at = NULL
WHERE id IN (
    SELECT id FROM decision_results
    WHERE raw_response IS NOT NULL
      AND raw_response_expires_at IS NOT NULL
      AND raw_response_expires_at < $1
    ORDER BY raw_response_expires_at ASC
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
"#;

pub(super) const LOAD_ARTICLE_TEXT_SQL: &str = r#"
SELECT title, body_text FROM articles WHERE id = $1
"#;

/// The statement `repo::run_event` uses, repeated here so decision-run's
/// events also get the bounded PostgreSQL transaction (and so removing the
/// feature touches no shared repository).
pub(super) const INSERT_RUN_EVENT_SQL: &str = r#"
INSERT INTO run_events (
    run_id, trace_id, stage, severity, event_kind,
    target_kind, target_id, message, context_json
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
RETURNING id
"#;

pub(super) const COUNT_OTHER_IDENTITY_PENDING_SQL: &str = r#"
SELECT COUNT(*) FROM decision_results
WHERE category_key = $1
  AND state = 'pending'
  AND NOT (policy_version = $2 AND execution_key = $3)
"#;
