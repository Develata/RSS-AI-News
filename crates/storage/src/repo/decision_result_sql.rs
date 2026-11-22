//! SQL for [`super::decision_result`]. Placeholders are `$N` on both
//! dialects; statements differ only where SQLite and PostgreSQL do (time
//! comparison of `articles.created_at`, `FOR UPDATE SKIP LOCKED`).

pub(super) const UPSERT_POLICY_SQL: &str = r#"
INSERT INTO decision_policies (policy_version, canonical_json, created_at)
VALUES ($1, $2, $3)
ON CONFLICT (policy_version) DO NOTHING
"#;

// Keyset generation (ADR 0011 §5): one page = the first `$n` candidates with
// `a.id > cursor`; the cursor advances to the last id *scanned* in the page,
// so a page whose candidates all conflict still moves forward. Each page is
// two statements in one transaction on SQLite (the write lock is taken first,
// so nothing commits in between): the page end, then the insert of every
// candidate in `(cursor, page_end]`. PostgreSQL uses one statement instead
// (`GENERATE_PAGE_PG_SQL`, R2-S19).

/// SQLite page end. `$2` is `sqlite_utc_seconds(cutoff)`; `created_at` is
/// normalised to the same whole-second UTC text (see
/// `repo::sqlite_utc_seconds`).
pub(super) const GENERATE_PAGE_END_SQLITE_SQL: &str = r#"
SELECT MAX(id) FROM (
    SELECT a.id
    FROM articles a
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE fs.category_key = $1
      AND a.state <> 'retired'
      AND replace(substr(a.created_at, 1, 19), 'T', ' ') >= $2
      AND a.id > $3
      AND NOT EXISTS (
          SELECT 1 FROM decision_results d
          WHERE d.article_id = a.id AND d.policy_version = $4 AND d.execution_key = $5
      )
    ORDER BY a.id ASC
    LIMIT $6
) page
"#;

/// SQLite: inserts the page `(cursor $9, page_end $10]`; `$8` as above.
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
  AND a.id > $9
  AND a.id <= $10
  AND NOT EXISTS (
      SELECT 1 FROM decision_results d
      WHERE d.article_id = a.id AND d.policy_version = $2 AND d.execution_key = $3
  )
ORDER BY a.id ASC
ON CONFLICT (article_id, policy_version, execution_key) DO NOTHING
"#;

/// PostgreSQL page (R2-S19): one statement, so the candidate ids, the insert
/// and the page end share one snapshot. Two statements under READ COMMITTED
/// let a smaller id committed between them slip into `(cursor, page_end]`
/// and the page insert more than `$9` rows (the ADR 0011 window cap). An id
/// committed below the cursor meanwhile is not lost: the cursor restarts at
/// 0 on every run (plan/18 §8.4). Returns `(page_end, inserted)`; `page_end`
/// is the last id *scanned* (NULL: no candidate), so an all-conflict page
/// still advances. `$7` is the whole-second cutoff as TIMESTAMPTZ.
pub(super) const GENERATE_PAGE_PG_SQL: &str = r#"
WITH page AS (
    SELECT a.id
    FROM articles a
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE fs.category_key = $1
      AND a.state <> 'retired'
      AND a.created_at >= $7
      AND a.id > $8
      AND NOT EXISTS (
          SELECT 1 FROM decision_results d
          WHERE d.article_id = a.id AND d.policy_version = $2 AND d.execution_key = $3
      )
    ORDER BY a.id ASC
    LIMIT $9
),
inserted AS (
    INSERT INTO decision_results (
        article_id, category_key, policy_version, execution_key, provider, requested_model,
        laya_max_len, state, created_at, updated_at
    )
    SELECT id, $1, $2, $3, $4, $5, $6, 'pending', $10, $10
    FROM page
    ORDER BY id ASC
    ON CONFLICT (article_id, policy_version, execution_key) DO NOTHING
    RETURNING 1
)
SELECT (SELECT MAX(id) FROM page) AS page_end,
       (SELECT COUNT(*) FROM inserted) AS inserted
"#;

/// SQLite: take the write lock first so the page-end read and the insert are
/// one serialised step (no read→write upgrade, no concurrent insert between).
pub(super) const LOCK_DECISION_RESULTS_SQLITE_SQL: &str =
    "UPDATE decision_results SET id = id WHERE 0";

/// SQLite: lease expiry compared by instant (`julianday()`), not by RFC3339
/// text (variable-width fractions sort out of instant order within a
/// second). `$2` is stored via `sqlite_stored_deadline` (UTC, ms rounded up),
/// `$9` is `sqlite_instant_cutoff(now)` (ms rounded down); `COALESCE` falls
/// back to the text comparison for text `julianday()` cannot parse. Same rule
/// for every SQLite deadline predicate below (feed_entries pattern, plan/02 §5.3).
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
      AND (lease_expires_at IS NULL
           OR COALESCE(julianday(lease_expires_at) < julianday($9), lease_expires_at < $9))
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

/// SQLite variant; `$5` is `sqlite_instant_cutoff(now)`.
pub(super) const RECLAIM_EXPIRED_SQLITE_SQL: &str = r#"
UPDATE decision_results
SET state = 'pending', lease_owner = NULL, lease_expires_at = NULL, updated_at = $1
WHERE category_key = $2 AND policy_version = $3 AND execution_key = $4
  AND state = 'running'
  AND COALESCE(julianday(lease_expires_at) < julianday($5), lease_expires_at < $5)
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

/// SQLite variant; `$6` is `sqlite_instant_cutoff(now)`.
pub(super) const TERMINALIZE_EXHAUSTED_SQLITE_SQL: &str = r#"
UPDATE decision_results
SET state = 'failed',
    last_error_kind = 'retry_budget_exhausted',
    last_error = COALESCE(last_error, 'retry budget exhausted'),
    completed_at = $1,
    updated_at = $1
WHERE category_key = $2 AND policy_version = $3 AND execution_key = $4
  AND state = 'pending'
  AND attempt_count >= $5
  AND (lease_expires_at IS NULL
       OR COALESCE(julianday(lease_expires_at) < julianday($6), lease_expires_at < $6))
"#;

/// Both columns are cleared, so a purged row is never selected again.
/// `$1` is `sqlite_instant_cutoff(now)`; the expiry is compared by instant.
/// The ordering keeps the text column (index order); it only picks which
/// expired rows go first.
pub(super) const PURGE_EXPIRED_RAW_SQLITE_SQL: &str = r#"
UPDATE decision_results
SET raw_response = NULL, raw_response_expires_at = NULL
WHERE id IN (
    SELECT id FROM decision_results
    WHERE raw_response IS NOT NULL
      AND raw_response_expires_at IS NOT NULL
      AND COALESCE(julianday(raw_response_expires_at) < julianday($1),
                   raw_response_expires_at < $1)
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

/// Only the first `$2` characters of the body: SQLite `substr` on TEXT and
/// PostgreSQL `substr` both count characters, so the prefix ends on a
/// character boundary.
pub(super) const LOAD_ARTICLE_TEXT_SQL: &str = r#"
SELECT title, substr(body_text, 1, $2) AS body_text FROM articles WHERE id = $1
"#;

/// SQLite (R2-S20): TEXT may hold an embedded NUL and `substr` stops there,
/// so the prefix also reports whether the stored bytes contain a NUL
/// (`instr` on the BLOB cast compares bytes). PostgreSQL TEXT cannot hold NUL.
pub(super) const LOAD_ARTICLE_TEXT_SQLITE_SQL: &str = r#"
SELECT title, substr(body_text, 1, $2) AS body_text,
       instr(CAST(body_text AS BLOB), x'00') > 0 AS has_nul
FROM articles WHERE id = $1
"#;

/// SQLite fallback for a NUL-containing body: the whole text, cut to a
/// character prefix in Rust.
pub(super) const LOAD_ARTICLE_FULL_TEXT_SQLITE_SQL: &str =
    "SELECT title, body_text FROM articles WHERE id = $1";

/// Whether any stored raw response has expired (one-row probe).
pub(super) const EXPIRED_RAW_EXISTS_SQLITE_SQL: &str = r#"
SELECT EXISTS (
    SELECT 1 FROM decision_results
    WHERE raw_response IS NOT NULL
      AND raw_response_expires_at IS NOT NULL
      AND COALESCE(julianday(raw_response_expires_at) < julianday($1),
                   raw_response_expires_at < $1)
)
"#;

pub(super) const EXPIRED_RAW_EXISTS_PG_SQL: &str = r#"
SELECT EXISTS (
    SELECT 1 FROM decision_results
    WHERE raw_response IS NOT NULL
      AND raw_response_expires_at IS NOT NULL
      AND raw_response_expires_at < $1
)
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
