//! feed_sources 共享 SQL 字符串。
//!
//! SQL 100% 跨方言等价（`$N` 占位符 + `ON CONFLICT` + `RETURNING`），由
//! [`super::feed_source_impl`] 的 sqlite_*/pg_* helper 共享；只有 sqlx 类型
//! 签名与 row decode 类型在实装层分叉。

pub(super) const UPSERT_FEED_SOURCE_RETURNING_ID_SQL: &str = r#"
INSERT INTO feed_sources (
    category_key, source_key, display_name, feed_url, feed_kind, status,
    priority, etag, last_modified, last_fetched_at, last_success_at,
    consecutive_failures, last_error, last_error_kind, config_version,
    created_at, updated_at
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
ON CONFLICT(category_key, source_key) DO UPDATE SET
    display_name = excluded.display_name,
    feed_url = excluded.feed_url,
    feed_kind = excluded.feed_kind,
    status = excluded.status,
    priority = excluded.priority,
    etag = excluded.etag,
    last_modified = excluded.last_modified,
    last_fetched_at = excluded.last_fetched_at,
    last_success_at = excluded.last_success_at,
    consecutive_failures = excluded.consecutive_failures,
    last_error = excluded.last_error,
    last_error_kind = excluded.last_error_kind,
    config_version = excluded.config_version,
    updated_at = excluded.updated_at
RETURNING id
"#;

/// 与 [`UPSERT_FEED_SOURCE_RETURNING_ID_SQL`] 同体，无 `RETURNING id`——
/// `upsert_with_lease_guard` 不需要返回 id 节省一次 RETURNING。
pub(super) const UPSERT_FEED_SOURCE_SQL: &str = r#"
INSERT INTO feed_sources (
    category_key, source_key, display_name, feed_url, feed_kind, status,
    priority, etag, last_modified, last_fetched_at, last_success_at,
    consecutive_failures, last_error, last_error_kind, config_version,
    created_at, updated_at
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
ON CONFLICT(category_key, source_key) DO UPDATE SET
    display_name = excluded.display_name,
    feed_url = excluded.feed_url,
    feed_kind = excluded.feed_kind,
    status = excluded.status,
    priority = excluded.priority,
    etag = excluded.etag,
    last_modified = excluded.last_modified,
    last_fetched_at = excluded.last_fetched_at,
    last_success_at = excluded.last_success_at,
    consecutive_failures = excluded.consecutive_failures,
    last_error = excluded.last_error,
    last_error_kind = excluded.last_error_kind,
    config_version = excluded.config_version,
    updated_at = excluded.updated_at
"#;

/// Config sync upsert (categories reindex, A14). Same 17 binds as
/// [`UPSERT_FEED_SOURCE_SQL`]; a new row is inserted as given, but on conflict
/// only config columns are written. Fetch runtime state (validators, failure
/// counters, last error) is reset only when the endpoint changed
/// (`feed_url` / `feed_kind`), exactly ingest's `needs_sync` rule;
/// `last_fetched_at` / `last_success_at` are always kept. SET expressions read
/// the pre-update row on both SQLite and PostgreSQL, so their order is free.
pub(super) const UPSERT_FEED_SOURCE_CONFIG_SQL: &str = r#"
INSERT INTO feed_sources (
    category_key, source_key, display_name, feed_url, feed_kind, status,
    priority, etag, last_modified, last_fetched_at, last_success_at,
    consecutive_failures, last_error, last_error_kind, config_version,
    created_at, updated_at
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
ON CONFLICT(category_key, source_key) DO UPDATE SET
    display_name = excluded.display_name,
    status = excluded.status,
    priority = excluded.priority,
    config_version = excluded.config_version,
    updated_at = excluded.updated_at,
    etag = CASE
        WHEN feed_sources.feed_url <> excluded.feed_url
          OR feed_sources.feed_kind <> excluded.feed_kind THEN NULL
        ELSE feed_sources.etag END,
    last_modified = CASE
        WHEN feed_sources.feed_url <> excluded.feed_url
          OR feed_sources.feed_kind <> excluded.feed_kind THEN NULL
        ELSE feed_sources.last_modified END,
    consecutive_failures = CASE
        WHEN feed_sources.feed_url <> excluded.feed_url
          OR feed_sources.feed_kind <> excluded.feed_kind THEN 0
        ELSE feed_sources.consecutive_failures END,
    last_error = CASE
        WHEN feed_sources.feed_url <> excluded.feed_url
          OR feed_sources.feed_kind <> excluded.feed_kind THEN NULL
        ELSE feed_sources.last_error END,
    last_error_kind = CASE
        WHEN feed_sources.feed_url <> excluded.feed_url
          OR feed_sources.feed_kind <> excluded.feed_kind THEN NULL
        ELSE feed_sources.last_error_kind END,
    feed_url = excluded.feed_url,
    feed_kind = excluded.feed_kind
"#;

pub(super) const SELECT_FEED_SOURCE_BY_ID_SQL: &str = r#"
SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
       priority, etag, last_modified, last_fetched_at, last_success_at,
       consecutive_failures, last_error, last_error_kind, config_version,
       created_at, updated_at
FROM feed_sources
WHERE id = $1
"#;

pub(super) const SELECT_FEED_SOURCE_BY_KEYS_SQL: &str = r#"
SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
       priority, etag, last_modified, last_fetched_at, last_success_at,
       consecutive_failures, last_error, last_error_kind, config_version,
       created_at, updated_at
FROM feed_sources
WHERE category_key = $1 AND source_key = $2
"#;

pub(super) const LIST_FEED_SOURCES_BY_CATEGORY_SQL: &str = r#"
SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
       priority, etag, last_modified, last_fetched_at, last_success_at,
       consecutive_failures, last_error, last_error_kind, config_version,
       created_at, updated_at
FROM feed_sources
WHERE category_key = $1 AND status = 'active'
ORDER BY priority ASC, source_key ASC
"#;

pub(super) const LIST_RECENT_FEED_SOURCE_HEALTH_SQL: &str = r#"
SELECT substr(source_key, 1, 256) AS source_key,
       priority,
       last_fetched_at,
       last_success_at,
       consecutive_failures,
       substr(last_error_kind, 1, 128) AS last_error_kind
FROM feed_sources
WHERE category_key = $1 AND status = 'active'
ORDER BY priority ASC, source_key ASC
LIMIT $2
"#;

pub(super) const LIST_FEED_SOURCES_ALL_SQL: &str = r#"
SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
       priority, etag, last_modified, last_fetched_at, last_success_at,
       consecutive_failures, last_error, last_error_kind, config_version,
       created_at, updated_at
FROM feed_sources
ORDER BY id ASC
"#;

pub(super) const MARK_FEED_SOURCE_ARCHIVED_SQL: &str = r#"
UPDATE feed_sources
SET status = 'archived', updated_at = $1
WHERE id = $2 AND status <> 'archived'
"#;

/// lease guard 用——把 reindex_jobs 行的 updated_at 顺手刷成 `now`
/// （fix9 heartbeat 语义）。`rows_affected == 1` ↔ lease 仍在手。
pub(super) const LEASE_GUARD_UPDATE_REINDEX_JOBS_SQL: &str = r#"
UPDATE reindex_jobs
SET updated_at = $1
WHERE id = $2 AND state = 'running' AND lease_owner = $3
"#;

pub(super) const UPDATE_FEED_SOURCE_AFTER_FETCH_SUCCESS_SQL: &str = r#"
UPDATE feed_sources
SET etag = $1,
    last_modified = $2,
    last_fetched_at = $3,
    last_success_at = $4,
    consecutive_failures = 0,
    last_error = NULL,
    last_error_kind = NULL,
    updated_at = $5
WHERE id = $6
"#;

/// 304 write-back: same as [`UPDATE_FEED_SOURCE_AFTER_FETCH_SUCCESS_SQL`]
/// except that a validator the 304 omitted keeps its stored value (a 304 need
/// not repeat `ETag` / `Last-Modified`, RFC 9110 §15.4.5). The 200 path keeps
/// replace semantics: the new representation's validators are the truth.
pub(super) const UPDATE_FEED_SOURCE_AFTER_NOT_MODIFIED_SQL: &str = r#"
UPDATE feed_sources
SET etag = COALESCE($1, etag),
    last_modified = COALESCE($2, last_modified),
    last_fetched_at = $3,
    last_success_at = $4,
    consecutive_failures = 0,
    last_error = NULL,
    last_error_kind = NULL,
    updated_at = $5
WHERE id = $6
"#;

pub(super) const UPDATE_FEED_SOURCE_AFTER_FETCH_FAILURE_SQL: &str = r#"
UPDATE feed_sources
SET last_fetched_at = $1,
    consecutive_failures = consecutive_failures + 1,
    last_error = $2,
    last_error_kind = $3,
    updated_at = $4
WHERE id = $5
"#;
