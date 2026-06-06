//! reindex_jobs 共享 SQL 字符串。
//!
//! W11-P3-C-2：除两条 claim 路径（PG 加 `FOR UPDATE SKIP LOCKED`，§6.4 契约）
//! 外，所有 const 跨方言完全等价。const 由 [`super::reindex_job_impl`] 的
//! sqlite_*/pg_* helper 共享。

pub(super) const SELECT_REINDEX_JOB_COLUMNS: &str = r#"
    id, target, rule_version_id, last_processed_id, total_estimated,
    state, error, aborted_reason, lease_owner, lease_expires_at,
    attempt_count, started_at, finished_at, created_at, updated_at
"#;

pub(super) const INSERT_RULE_VERSION_PENDING_SQL: &str = r#"
INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status)
VALUES ($1, $2, $3, $4, 'pending')
RETURNING id
"#;

pub(super) const INSERT_REINDEX_JOB_PENDING_SQL: &str = r#"
INSERT INTO reindex_jobs (
    target, rule_version_id, state, attempt_count,
    created_at, updated_at
)
VALUES ($1, $2, 'pending', 0, $3, $4)
RETURNING id
"#;

pub(super) const COMPLETE_WITHOUT_CLAIM_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'completed',
    lease_owner = NULL,
    lease_expires_at = NULL,
    finished_at = $1,
    updated_at = $2
WHERE id = $3 AND state = 'pending'
"#;

pub(super) const FINISH_REINDEX_UPDATE_JOB_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'completed',
    lease_owner = NULL,
    lease_expires_at = NULL,
    finished_at = $1,
    updated_at = $2
WHERE id = $3 AND state = 'running' AND lease_owner = $4
"#;

pub(super) const FINISH_REINDEX_DEMOTE_ACTIVE_SQL: &str = r#"
UPDATE rule_versions
SET status = 'superseded',
    retired_at = $1
WHERE kind = $2 AND status = 'active' AND id != $3
RETURNING id
"#;

pub(super) const FINISH_REINDEX_PROMOTE_PENDING_SQL: &str = r#"
UPDATE rule_versions
SET status = 'active'
WHERE id = $1 AND kind = $2 AND status = 'pending'
"#;

/// SQLite `claim_pending`：子查询无 `FOR UPDATE SKIP LOCKED`（语法不支持，
/// 整库写锁本身串行化并发）。
pub(super) const CLAIM_PENDING_SQLITE_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    started_at = COALESCE(started_at, $3),
    attempt_count = attempt_count + 1,
    updated_at = $4
WHERE id = (
    SELECT id FROM reindex_jobs
    WHERE state = 'pending'
      AND (lease_expires_at IS NULL OR lease_expires_at < $5)
    ORDER BY created_at ASC, id ASC
    LIMIT 1
)
RETURNING id, target, rule_version_id, last_processed_id, attempt_count
"#;

/// PG `claim_pending`：§6.4 契约——子查询 `FOR UPDATE SKIP LOCKED` 让多 worker
/// 并发抢同一池时各自拿到不同候选，避免行锁等待序列化。
///
/// PG 语法限制：`FOR UPDATE` 不能放在标量子查询 `= (...)` 里；用 `IN (...)`。
pub(super) const CLAIM_PENDING_PG_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    started_at = COALESCE(started_at, $3),
    attempt_count = attempt_count + 1,
    updated_at = $4
WHERE id IN (
    SELECT id FROM reindex_jobs
    WHERE state = 'pending'
      AND (lease_expires_at IS NULL OR lease_expires_at < $5)
    ORDER BY created_at ASC, id ASC
    LIMIT 1
    FOR UPDATE SKIP LOCKED
)
RETURNING id, target, rule_version_id, last_processed_id, attempt_count
"#;

/// SQLite `claim_by_id`：按 id 寻址 + state/lease 谓词；UPDATE 单语句 row lock 足够。
pub(super) const CLAIM_BY_ID_SQLITE_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    started_at = COALESCE(started_at, $3),
    attempt_count = attempt_count + 1,
    updated_at = $4
WHERE id = $5
  AND state = 'pending'
  AND (lease_expires_at IS NULL OR lease_expires_at < $6)
RETURNING id, target, rule_version_id, last_processed_id, attempt_count
"#;

/// PG `claim_by_id`：与 SQLite 等价 + 子查询 `FOR UPDATE SKIP LOCKED`，
/// 让两 worker 同时 claim 同一 id 时第二个立即拿到锁失败 → `None`
/// （与"已被 claim"语义一致）。
pub(super) const CLAIM_BY_ID_PG_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'running',
    lease_owner = $1,
    lease_expires_at = $2,
    started_at = COALESCE(started_at, $3),
    attempt_count = attempt_count + 1,
    updated_at = $4
WHERE id IN (
    SELECT id FROM reindex_jobs
    WHERE id = $5
      AND state = 'pending'
      AND (lease_expires_at IS NULL OR lease_expires_at < $6)
    FOR UPDATE SKIP LOCKED
)
RETURNING id, target, rule_version_id, last_processed_id, attempt_count
"#;

pub(super) const ADVANCE_CHECKPOINT_SQL: &str = r#"
UPDATE reindex_jobs
SET last_processed_id = $1, updated_at = $2
WHERE id = $3 AND state = 'running' AND lease_owner = $4
"#;

pub(super) const ASSERT_LEASE_HELD_SQL: &str = r#"
UPDATE reindex_jobs
SET updated_at = $1
WHERE id = $2 AND state = 'running' AND lease_owner = $3
"#;

pub(super) const ADVANCE_TO_COMPLETED_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'completed',
    lease_owner = NULL,
    lease_expires_at = NULL,
    finished_at = $1,
    updated_at = $2
WHERE id = $3 AND state = 'running' AND lease_owner = $4
"#;

pub(super) const MARK_FAILED_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'failed',
    error = $1,
    lease_owner = NULL,
    lease_expires_at = NULL,
    finished_at = $2,
    updated_at = $3
WHERE id = $4 AND state = 'running' AND lease_owner = $5
"#;

pub(super) const ABORT_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'aborted',
    aborted_reason = $1,
    lease_owner = NULL,
    lease_expires_at = NULL,
    finished_at = $2,
    updated_at = $3
WHERE id = $4 AND state IN ('pending', 'running')
"#;

pub(super) const RECLAIM_EXPIRED_LEASES_SQL: &str = r#"
UPDATE reindex_jobs
SET state = 'pending',
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE state = 'running'
  AND lease_expires_at IS NOT NULL
  AND lease_expires_at < $2
"#;
