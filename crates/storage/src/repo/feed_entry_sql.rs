//! feed_entries 共享 SQL 字符串。
//!
//! W11-P3-E-2：除 `claim_pending_fetch`（PG 加 `FOR UPDATE SKIP LOCKED`，§6.4
//! 契约）外所有 const 跨方言完全等价。const 由 [`super::feed_entry_impl`] 的
//! sqlite_*/pg_* helper 共享。

pub(super) const INSERT_FEED_ENTRY_SQL: &str = r#"
INSERT INTO feed_entries (
    source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
    summary_raw, published_at, discovered_at, state, dedup_decision
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending_fetch', 'fresh')
ON CONFLICT(source_id, feed_entry_uid) DO NOTHING
RETURNING id
"#;

pub(super) const EXISTS_BY_LINK_HASH_SQL: &str =
    "SELECT CASE WHEN EXISTS(SELECT 1 FROM feed_entries WHERE link_hash = $1) THEN 1 ELSE 0 END";

pub(super) const SELECT_FEED_ENTRY_BY_ID_SQL: &str = r#"
SELECT id, source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
       summary_raw, published_at, discovered_at, state, dedup_decision,
       article_id, lease_owner, lease_expires_at, attempt_count, last_error,
       last_error_kind, created_at, updated_at
FROM feed_entries
WHERE id = $1
"#;

/// SQLite claim：子查询无 `FOR UPDATE`（语法不支持，整库写锁兜底）。
pub(super) const CLAIM_PENDING_FETCH_SQLITE_SQL: &str = r#"
UPDATE feed_entries
SET state = 'fetching',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    updated_at = $3
WHERE id IN (
    SELECT id FROM feed_entries
    WHERE state = 'pending_fetch'
      AND (lease_expires_at IS NULL OR lease_expires_at < $4)
      AND attempt_count < $5
    ORDER BY discovered_at ASC
    LIMIT $6
)
RETURNING id, source_id, normalized_link, link_hash, title_raw,
          discovered_at, attempt_count
"#;

/// PG claim：§6.4 契约——子查询 `FOR UPDATE SKIP LOCKED`，让 ingest 多 worker
/// 并发 claim 同一 pending_fetch 池时各自拿到不同候选；否则会序列化等待
/// row lock，等价单 worker。
pub(super) const CLAIM_PENDING_FETCH_PG_SQL: &str = r#"
UPDATE feed_entries
SET state = 'fetching',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1,
    updated_at = $3
WHERE id IN (
    SELECT id FROM feed_entries
    WHERE state = 'pending_fetch'
      AND (lease_expires_at IS NULL OR lease_expires_at < $4)
      AND attempt_count < $5
    ORDER BY discovered_at ASC
    LIMIT $6
    FOR UPDATE SKIP LOCKED
)
RETURNING id, source_id, normalized_link, link_hash, title_raw,
          discovered_at, attempt_count
"#;

pub(super) const RELEASE_SUCCESS_SQL: &str = r#"
UPDATE feed_entries
SET state = 'persisted', article_id = $1, lease_owner = NULL,
    lease_expires_at = NULL, last_error = NULL, last_error_kind = NULL,
    updated_at = $2
WHERE id = $3 AND lease_owner = $4
"#;

pub(super) const RELEASE_FEED_FAILURE_SQL: &str = r#"
UPDATE feed_entries
SET state = $1, lease_owner = NULL, lease_expires_at = NULL,
    last_error = $2, last_error_kind = $3, updated_at = $4
WHERE id = $5 AND lease_owner = $6
"#;

/// W15 §3 折叠：retryable 失败按预算决定回队 / 转终态，规则收口在 SQL。
/// `RETURNING state` 供调用方判定走向。claim 过滤发生在自增前，故 release
/// 时 `attempt_count >= max` 当且仅当本次是预算内最后一次尝试。
pub(super) const RELEASE_FEED_RETRYABLE_FAILURE_SQL: &str = r#"
UPDATE feed_entries
SET state = CASE WHEN attempt_count >= $1 THEN 'failed' ELSE 'pending_fetch' END,
    lease_owner = NULL, lease_expires_at = NULL,
    last_error = $2, last_error_kind = $3, updated_at = $4
WHERE id = $5 AND lease_owner = $6
RETURNING state
"#;

/// 设计 §5.5 写明 reclaim 不改 state，但 §5.1 只领取 pending_fetch。
/// 这里按 W4b 指令采用方案 A：过期 fetching/extracting 回到 pending_fetch。
pub(super) const RECLAIM_FEED_ENTRY_LEASES_SQL: &str = r#"
UPDATE feed_entries
SET state = 'pending_fetch',
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE lease_expires_at IS NOT NULL
  AND lease_expires_at < $2
  AND state IN ('fetching', 'extracting')
"#;

pub(super) const RELEASE_DEDUP_SKIPPED_SQL: &str = r#"
UPDATE feed_entries
SET state = 'dedup_skipped',
    dedup_decision = $1,
    article_id = $2,
    lease_owner = NULL,
    lease_expires_at = NULL,
    last_error = NULL,
    last_error_kind = NULL,
    updated_at = $3
WHERE id = $4 AND lease_owner = $5
"#;

pub(super) const RELEASE_FALLBACK_PERSISTED_SQL: &str = r#"
UPDATE feed_entries
SET state = 'fallback_persisted',
    article_id = $1,
    lease_owner = NULL,
    lease_expires_at = NULL,
    last_error = NULL,
    last_error_kind = NULL,
    updated_at = $2
WHERE id = $3 AND lease_owner = $4
"#;

/// W15 §4 sweep：预算耗尽且 claim 永远不会再领取的 pending_fetch 行 → 终态。
/// COALESCE 保留行上既有真实错误（retryable release 已写过），仅对从未留过
/// 错误的行落兜底文案。
pub(super) const TERMINALIZE_EXHAUSTED_FEED_SQL: &str = r#"
UPDATE feed_entries
SET state = 'failed',
    last_error = COALESCE(last_error, 'retry budget exhausted'),
    last_error_kind = COALESCE(last_error_kind, 'retry_budget_exhausted'),
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $1
WHERE state = 'pending_fetch'
  AND attempt_count >= $2
  AND (lease_expires_at IS NULL OR lease_expires_at < $3)
"#;

pub(super) const COUNT_FEED_ENTRIES_IN_WINDOW_SQL: &str = r#"
SELECT COUNT(*) FROM feed_entries
WHERE ($1 IS NULL OR created_at >= $1)
  AND ($2 IS NULL OR created_at < $2)
"#;

// 重置目标必须是 'pending_fetch'：extract 的 claim 只认 'pending_fetch'
// （见 CLAIM_PENDING_FETCH_*），且 INSERT 新条目也直接进 'pending_fetch'。
// 'discovered' 在当前管线中无任何消费者（claim/晋升都不认它），重置到该
// 状态会让条目卡死、永不被重抓——backfill extract 因此曾是空操作。
pub(super) const RESET_FAILED_IN_WINDOW_SQL: &str = r#"
UPDATE feed_entries
SET state = 'pending_fetch',
    attempt_count = 0,
    last_error = NULL,
    last_error_kind = NULL,
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = $3
WHERE state = 'failed'
  AND ($1 IS NULL OR created_at >= $1)
  AND ($2 IS NULL OR created_at < $2)
"#;

pub(super) const LIST_FOR_LINK_HASH_REINDEX_SQL: &str = r#"
SELECT id, normalized_link, link_hash
FROM feed_entries
WHERE id > $1
ORDER BY id ASC
LIMIT $2
"#;

pub(super) const UPDATE_LINK_HASH_SQL: &str = r#"
UPDATE feed_entries
SET link_hash = $1, updated_at = $2
WHERE id = $3
"#;
