//! publish_records 共享 SQL 字符串 + claim 辅助。
//!
//! W11-P3-C-4：所有 const SQL 跨方言完全等价（已用 `$N` 占位符 +
//! ON CONFLICT + RETURNING），仅 `claim_publish` 子查询在 PG 必须加
//! `FOR UPDATE SKIP LOCKED`（§6.4 契约），故按 backend 分裂。

use sqlx::{PgPool, SqlitePool};

use crate::{ClaimRequest, StorageError};

// ── SQLite instant-correct lease comparisons ───────────────────────────
//
// SQLite stores OffsetDateTime as RFC3339 TEXT with a variable-length
// fraction, so a lexicographic `<` is not an instant `<` within one second
// (`'…20.5Z' < '…20Z'`). Lease predicates therefore compare `julianday()`
// values, as feed_entries does (feed_entry_sql.rs, plan/02 §5.3).
// `julianday()` rounds to the nearest millisecond, so the stored deadline is
// written in UTC rounded *up* to a whole millisecond and the compared `now` is
// rounded *down*: both sides are then exact and a lease is never treated as
// expired before its instant. `julianday()` returns NULL for text it cannot
// parse (e.g. some valid offsets such as `+15:00`), so each predicate falls
// back to the previous text comparison via `COALESCE`.
//
// PostgreSQL compares `timestamptz` by instant and needs no change.
pub(super) use super::{sqlite_instant_cutoff as sqlite_lease_cutoff, sqlite_stored_deadline};

use super::publish_record::ClaimedPublishRecord;

pub(super) async fn claim_publish_sqlite(
    pool: &SqlitePool,
    request: &ClaimRequest,
    from: &str,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    sqlx::query_as::<_, ClaimedPublishRecord>(CLAIM_PUBLISH_SQLITE_SQL)
        .bind(&request.owner)
        .bind(sqlite_stored_deadline(request.lease_expires_at))
        .bind(request.now)
        .bind(from)
        .bind(sqlite_lease_cutoff(request.now))
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn claim_publish_pg(
    pool: &PgPool,
    request: &ClaimRequest,
    from: &str,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    sqlx::query_as::<_, ClaimedPublishRecord>(CLAIM_PUBLISH_PG_SQL)
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
        .bind(from)
        .bind(request.now)
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) const SELECT_PUBLISH_RECORD_BY_ID: &str = r#"
SELECT id, idempotency_key, category_key, report_date, target_timezone,
       render_version, selection_policy_version, state, snapshot_frozen_at,
       rendered_at, local_stored_at, remote_published_at, local_path,
       remote_target, commit_sha, lease_owner, lease_expires_at, attempt_count,
       last_error, last_error_kind, created_at, updated_at
FROM publish_records
WHERE id = $1
"#;

pub(super) const SELECT_PUBLISH_RECORD_BY_IDEMPOTENCY_KEY: &str = r#"
SELECT id, idempotency_key, category_key, report_date, target_timezone,
       render_version, selection_policy_version, state, snapshot_frozen_at,
       rendered_at, local_stored_at, remote_published_at, local_path,
       remote_target, commit_sha, lease_owner, lease_expires_at, attempt_count,
       last_error, last_error_kind, created_at, updated_at
FROM publish_records
WHERE idempotency_key = $1
"#;

pub(super) const CREATE_IF_NEW_SQL: &str = r#"
INSERT INTO publish_records (
    idempotency_key, category_key, report_date, target_timezone,
    render_version, selection_policy_version, remote_target
)
VALUES ($1, $2, $3, $4, $5, $6, $7)
ON CONFLICT(idempotency_key) DO NOTHING
RETURNING id
"#;

/// SQLite claim：子查询无 `FOR UPDATE`（语法不支持，整库写锁兜底）。
/// lease 过期按时刻比较（见上方 julianday 说明）：`$2` 向上、`$5` 向下取整到毫秒。
/// `next_attempt_at`（限流后的最早重试时刻，R1-P-S12）同样按时刻比较，未到不领取。
const CLAIM_PUBLISH_SQLITE_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = $1, lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    stage_attempt_count = stage_attempt_count + 1, updated_at = $3
WHERE id IN (
    SELECT id FROM publish_records
    WHERE state = $4
      AND (lease_expires_at IS NULL
           OR COALESCE(julianday(lease_expires_at) < julianday($5), lease_expires_at < $5))
      AND (next_attempt_at IS NULL
           OR COALESCE(julianday(next_attempt_at) <= julianday($5), next_attempt_at <= $5))
      AND stage_attempt_count < $6
    ORDER BY id ASC
    LIMIT $7
)
RETURNING id, idempotency_key, category_key, report_date, target_timezone,
          render_version, selection_policy_version, state, remote_target,
          attempt_count, rendered_at
"#;

/// PG claim：§6.4 契约——子查询 `FOR UPDATE SKIP LOCKED`，让 publish flow
/// 多 worker 并发 claim 同状态的多个 publish_records 时各自拿到不同候选；
/// 否则会序列化等待 row lock，等价单 worker。
const CLAIM_PUBLISH_PG_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = $1, lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    stage_attempt_count = stage_attempt_count + 1, updated_at = $3
WHERE id IN (
    SELECT id FROM publish_records
    WHERE state = $4
      AND (lease_expires_at IS NULL OR lease_expires_at < $5)
      AND (next_attempt_at IS NULL OR next_attempt_at <= $5)
      AND stage_attempt_count < $6
    ORDER BY id ASC
    LIMIT $7
    FOR UPDATE SKIP LOCKED
)
RETURNING id, idempotency_key, category_key, report_date, target_timezone,
          render_version, selection_policy_version, state, remote_target,
          attempt_count, rendered_at
"#;

/// Pending claim limited to one category (R1-P-S15): the freeze selects
/// candidates for the caller's category, so it must not claim another
/// category's record. Same filters as [`CLAIM_PUBLISH_SQLITE_SQL`] (`$4` is
/// always `'pending'`), plus `category_key = $8`.
const CLAIM_PENDING_IN_CATEGORY_SQLITE_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = $1, lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    stage_attempt_count = stage_attempt_count + 1, updated_at = $3
WHERE id IN (
    SELECT id FROM publish_records
    WHERE state = $4
      AND category_key = $8
      AND (lease_expires_at IS NULL
           OR COALESCE(julianday(lease_expires_at) < julianday($5), lease_expires_at < $5))
      AND (next_attempt_at IS NULL
           OR COALESCE(julianday(next_attempt_at) <= julianday($5), next_attempt_at <= $5))
      AND stage_attempt_count < $6
    ORDER BY id ASC
    LIMIT $7
)
RETURNING id, idempotency_key, category_key, report_date, target_timezone,
          render_version, selection_policy_version, state, remote_target,
          attempt_count, rendered_at
"#;

/// PG variant of [`CLAIM_PENDING_IN_CATEGORY_SQLITE_SQL`].
const CLAIM_PENDING_IN_CATEGORY_PG_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = $1, lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    stage_attempt_count = stage_attempt_count + 1, updated_at = $3
WHERE id IN (
    SELECT id FROM publish_records
    WHERE state = $4
      AND category_key = $8
      AND (lease_expires_at IS NULL OR lease_expires_at < $5)
      AND (next_attempt_at IS NULL OR next_attempt_at <= $5)
      AND stage_attempt_count < $6
    ORDER BY id ASC
    LIMIT $7
    FOR UPDATE SKIP LOCKED
)
RETURNING id, idempotency_key, category_key, report_date, target_timezone,
          render_version, selection_policy_version, state, remote_target,
          attempt_count, rendered_at
"#;

pub(super) async fn claim_pending_in_category_sqlite(
    pool: &SqlitePool,
    request: &ClaimRequest,
    category_key: &str,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    sqlx::query_as::<_, ClaimedPublishRecord>(CLAIM_PENDING_IN_CATEGORY_SQLITE_SQL)
        .bind(&request.owner)
        .bind(sqlite_stored_deadline(request.lease_expires_at))
        .bind(request.now)
        .bind("pending")
        .bind(sqlite_lease_cutoff(request.now))
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .bind(category_key)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) async fn claim_pending_in_category_pg(
    pool: &PgPool,
    request: &ClaimRequest,
    category_key: &str,
) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
    sqlx::query_as::<_, ClaimedPublishRecord>(CLAIM_PENDING_IN_CATEGORY_PG_SQL)
        .bind(&request.owner)
        .bind(request.lease_expires_at)
        .bind(request.now)
        .bind("pending")
        .bind(request.now)
        .bind(i64::from(request.max_attempts))
        .bind(i64::from(request.batch_size))
        .bind(category_key)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

pub(super) const ADVANCE_SNAPSHOT_SQL: &str = "UPDATE publish_records SET state = $1, snapshot_frozen_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, stage_attempt_count = 0, next_attempt_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_RENDERED_SQL: &str = "UPDATE publish_records SET state = $1, rendered_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, stage_attempt_count = 0, next_attempt_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_LOCAL_SQL: &str = "UPDATE publish_records SET state = $1, local_stored_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, stage_attempt_count = 0, next_attempt_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
pub(super) const ADVANCE_REMOTE_SQL: &str = "UPDATE publish_records SET state = $1, remote_published_at = $2, local_path = COALESCE($3, local_path), remote_target = COALESCE($4, remote_target), commit_sha = COALESCE($5, commit_sha), lease_owner = NULL, lease_expires_at = NULL, stage_attempt_count = 0, next_attempt_at = NULL, last_error = NULL, last_error_kind = NULL, updated_at = $6 WHERE id = $7 AND lease_owner = $8 AND state = $9";
/// W15 §3 折叠：retryable 失败按预算决定维持原阶段态（清 lease 后等下轮
/// claim）/ 转终态 `failed`，规则收口在 SQL。`RETURNING state` 供调用方判定
/// 走向。预算按阶段计（`stage_attempt_count`，ADR 0012）：claim 自增、阶段推进清零。
/// `$7` = 最早重试时刻（限流时为 Retry-After / reset，否则 NULL；R1-P-S12），claim 在此之前不领取。
pub(super) const RELEASE_PUBLISH_RETRYABLE_FAILURE_SQL: &str = r#"
UPDATE publish_records
SET state = CASE WHEN stage_attempt_count >= $1 THEN 'failed' ELSE state END,
    lease_owner = NULL, lease_expires_at = NULL,
    last_error = $2, last_error_kind = $3, updated_at = $4,
    next_attempt_at = $7
WHERE id = $5 AND lease_owner = $6
RETURNING state
"#;
pub(super) const RELEASE_PERMANENT_FAILURE_SQL: &str = "UPDATE publish_records SET state = 'failed', lease_owner = NULL, lease_expires_at = NULL, last_error = $1, last_error_kind = $2, updated_at = $3 WHERE id = $4 AND lease_owner = $5";
/// Freeze found no candidates: return the record to claimable `pending`
/// instead of `failed`, and refund the attempt counted by the claim — an empty
/// snapshot is not a publish attempt, so repeated empty runs must not exhaust
/// the retry budget. Guarded by owner + state like every release.
pub(super) const RELEASE_EMPTY_SNAPSHOT_SQL: &str = "UPDATE publish_records SET lease_owner = NULL, lease_expires_at = NULL, attempt_count = CASE WHEN attempt_count > 0 THEN attempt_count - 1 ELSE 0 END, stage_attempt_count = CASE WHEN stage_attempt_count > 0 THEN stage_attempt_count - 1 ELSE 0 END, last_error = 'no publish candidates', last_error_kind = 'snapshot_empty', updated_at = $1 WHERE id = $2 AND lease_owner = $3 AND state = 'pending'";
/// Freeze lost an article to another snapshot (or the article changed
/// state) after selection. The record goes back to claimable `pending` with
/// its lease released, and the claim is refunded on both counters: losing a
/// race is not a publish attempt, so it must not exhaust the stage budget.
/// The next run re-selects without the contested article.
pub(super) const RELEASE_FREEZE_CONFLICT_SQL: &str = "UPDATE publish_records SET lease_owner = NULL, lease_expires_at = NULL, attempt_count = CASE WHEN attempt_count > 0 THEN attempt_count - 1 ELSE 0 END, stage_attempt_count = CASE WHEN stage_attempt_count > 0 THEN stage_attempt_count - 1 ELSE 0 END, last_error = $1, last_error_kind = $2, updated_at = $3 WHERE id = $4 AND lease_owner = $5 AND state = 'pending'";
/// A claimed record whose snapshot shares an article with a winning snapshot
/// (P-B1): release the lease in the current stage, refund the claim on both
/// counters (no write happened, so it is not an attempt) and record why. The
/// state is kept so the record can continue if the winner later fails.
pub(super) const RELEASE_OWNERSHIP_CONFLICT_SQL: &str = "UPDATE publish_records SET lease_owner = NULL, lease_expires_at = NULL, attempt_count = CASE WHEN attempt_count > 0 THEN attempt_count - 1 ELSE 0 END, stage_attempt_count = CASE WHEN stage_attempt_count > 0 THEN stage_attempt_count - 1 ELSE 0 END, last_error = $1, last_error_kind = $2, updated_at = $3 WHERE id = $4 AND lease_owner = $5 AND state = $6";
/// A remote attempt whose cost is not charged to the stage (R3F-B2, R3F-S12):
/// a write whose outcome is unknown and whose read-only HEAD verification did
/// not complete, or a rate-limited attempt that persists GitHub's deadline.
/// Like [`RELEASE_OWNERSHIP_CONFLICT_SQL`] it keeps the state (the record keeps
/// owning its snapshot's articles), clears the lease and refunds the claim on
/// both counters; in addition `$7` = the earliest next claim
/// (`next_attempt_at`, NULL = no wait).
pub(super) const RELEASE_DEFERRED_RETRY_SQL: &str = "UPDATE publish_records SET lease_owner = NULL, lease_expires_at = NULL, attempt_count = CASE WHEN attempt_count > 0 THEN attempt_count - 1 ELSE 0 END, stage_attempt_count = CASE WHEN stage_attempt_count > 0 THEN stage_attempt_count - 1 ELSE 0 END, last_error = $1, last_error_kind = $2, updated_at = $3, next_attempt_at = $7 WHERE id = $4 AND lease_owner = $5 AND state = $6";
pub(super) const RECLAIM_PUBLISH_LEASES_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = NULL, lease_expires_at = NULL, updated_at = $1
WHERE lease_expires_at IS NOT NULL
  AND lease_expires_at < $2
  AND state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local')
"#;

/// SQLite 变体：按时刻比较 lease（julianday，`$2` 为向下取整到毫秒的 now）。
pub(super) const RECLAIM_PUBLISH_LEASES_SQLITE_SQL: &str = r#"
UPDATE publish_records
SET lease_owner = NULL, lease_expires_at = NULL, updated_at = $1
WHERE lease_expires_at IS NOT NULL
  AND COALESCE(julianday(lease_expires_at) < julianday($2), lease_expires_at < $2)
  AND state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local')
"#;

/// W15 §4 sweep：预算耗尽且 claim 永远不会再领取的阶段态行 → 终态。
/// 预算按阶段计（`stage_attempt_count`，ADR 0012），与 claim 过滤严格互补。
/// COALESCE 保留行上既有真实错误，仅对从未留过错误的行落兜底文案。
pub(super) const TERMINALIZE_EXHAUSTED_PUBLISH_SQL: &str = r#"
UPDATE publish_records
SET state = 'failed',
    last_error = COALESCE(last_error, 'retry budget exhausted'),
    last_error_kind = COALESCE(last_error_kind, 'retry_budget_exhausted'),
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local')
  AND stage_attempt_count >= $2
  AND (lease_expires_at IS NULL OR lease_expires_at < $3)
"#;

/// SQLite 变体：lease 过期按时刻比较（julianday，`$3` 为向下取整到毫秒的 now）。
pub(super) const TERMINALIZE_EXHAUSTED_PUBLISH_SQLITE_SQL: &str = r#"
UPDATE publish_records
SET state = 'failed',
    last_error = COALESCE(last_error, 'retry budget exhausted'),
    last_error_kind = COALESCE(last_error_kind, 'retry_budget_exhausted'),
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local')
  AND stage_attempt_count >= $2
  AND (lease_expires_at IS NULL
       OR COALESCE(julianday(lease_expires_at) < julianday($3), lease_expires_at < $3))
"#;
pub(super) const PROMOTE_ARTICLE_PUBLISHED_SQL: &str = "UPDATE articles SET state = 'published', updated_at = $1 WHERE id = $2 AND state = 'ready_for_publish'";

/// codex P3-C 评审 LOW-1 修复：PG `id = ANY($2)` 批量 UPDATE + `RETURNING id`，
/// 避免逐行 N+1 拉长事务时间与行锁持有窗口。SQLite 无数组 bind 仍走逐行。
///
/// 调用方通过比对 `RETURNING` 返回的 id 集合与 input promote_article_ids 列表
/// 找出第一个未 promote 的 id（保留 `ArticleStateConflict { article_id }` 的
/// 单 id 语义）。
pub(super) const PROMOTE_ARTICLES_PUBLISHED_BATCH_PG_SQL: &str = "UPDATE articles SET state = 'published', updated_at = $1 WHERE id = ANY($2) AND state = 'ready_for_publish' RETURNING id";
