mod common;

use std::time::Duration as StdDuration;

use rss_ai_news_storage::{
    FeedEntryRepo, FeedEntryRepository, RecentFeedEntryFilter, StoragePool,
    build_sqlite_read_only_pool, ensure_migration_state_exact,
    repo::feed_entry::RECENT_ENTRIES_SQLITE_QUERY_FOR_DIAGNOSTICS,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

#[tokio::test]
async fn recent_entries_filters_category_and_active_sources() {
    let (_dir, pool) = common::make_test_pool().await;
    let config = common::insert_rule(&pool, "config", "recent-filter", "sha-filter").await;
    let active_a = insert_source(&pool, config, "category-a", "active-a", "active", 10).await;
    let paused_a = insert_source(&pool, config, "category-a", "paused-a", "paused", 20).await;
    let active_b = insert_source(&pool, config, "category-b", "active-b", "active", 10).await;
    let now = fixed_time(100);
    insert_entry(&pool, active_a, "active-a", now, "pending_fetch").await;
    insert_entry(&pool, paused_a, "paused-a", now, "pending_fetch").await;
    insert_entry(&pool, active_b, "active-b", now, "pending_fetch").await;

    let rows = recent(&pool, "category-a", fixed_time(0), 10).await;

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source_key, "active-a");
}

#[tokio::test]
async fn recent_entries_uses_inclusive_discovered_after() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = seed_recent_source(&pool, "inclusive", 10).await;
    let boundary = fixed_time(1_000);
    insert_entry(
        &pool,
        source,
        "before",
        boundary - Duration::seconds(1),
        "pending_fetch",
    )
    .await;
    insert_entry(&pool, source, "at", boundary, "pending_fetch").await;
    insert_entry(
        &pool,
        source,
        "after",
        boundary + Duration::seconds(1),
        "pending_fetch",
    )
    .await;

    let rows = recent(&pool, "daily-math", boundary, 10).await;
    let titles = rows
        .iter()
        .map(|row| row.title.as_str())
        .collect::<Vec<_>>();

    assert_eq!(titles, vec!["after", "at"]);
}

#[tokio::test]
async fn recent_entries_uses_inclusive_optional_published_after() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = seed_recent_source(&pool, "published-cutoff", 10).await;
    let discovered_at = fixed_time(5_000);
    let cases = [
        ("after", Some("2024-01-01T00:00:00.5Z")),
        ("at-offset", Some("2024-01-01T08:00:00.25+08:00")),
        ("before", Some("2024-01-01T00:00:00.249Z")),
        ("missing", None),
    ];
    for (title, published_at) in cases {
        let id = insert_entry(&pool, source, title, discovered_at, "pending_fetch").await;
        if let Some(published_at) = published_at {
            sqlx::query("UPDATE feed_entries SET published_at = ? WHERE id = ?")
                .bind(published_at)
                .bind(id)
                .execute(&pool)
                .await
                .expect("set published_at fixture");
        }
    }
    let published_after = OffsetDateTime::parse("2024-01-01T00:00:00.25Z", &Rfc3339).unwrap();

    let rows = FeedEntryRepo::new(pool.clone())
        .list_recent(&RecentFeedEntryFilter {
            category_key: "daily-math".to_string(),
            discovered_after: fixed_time(0),
            published_after: Some(published_after),
            max_rows: 10,
        })
        .await
        .expect("filter by published_at instant");
    let mut titles = rows
        .iter()
        .map(|row| row.title.as_str())
        .collect::<Vec<_>>();
    titles.sort_unstable();
    assert_eq!(titles, vec!["after", "at-offset"]);
    assert!(rows.iter().all(|row| row.published_at.is_some()));
}

#[tokio::test]
async fn recent_entries_sqlite_orders_fractional_and_offset_timestamps_by_instant() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = seed_recent_source(&pool, "timestamp-parity", 10).await;
    for (title, timestamp) in [
        ("before", "2023-12-31T23:59:59.999999999Z"),
        ("zero", "2024-01-01T00:00:00Z"),
        ("offset-quarter", "2024-01-01T08:00:00.25+08:00"),
        ("half", "2024-01-01T00:00:00.5Z"),
    ] {
        insert_entry_raw_timestamp(&pool, source, title, timestamp).await;
    }
    let zero = OffsetDateTime::parse("2024-01-01T00:00:00Z", &Rfc3339).unwrap();
    let quarter = OffsetDateTime::parse("2024-01-01T00:00:00.25Z", &Rfc3339).unwrap();

    let rows = recent(&pool, "daily-math", zero, 10).await;
    assert_eq!(
        rows.iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        vec!["half", "offset-quarter", "zero"]
    );

    let rows = recent(&pool, "daily-math", quarter, 10).await;
    assert_eq!(
        rows.iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        vec!["half", "offset-quarter"]
    );
}

#[tokio::test]
async fn recent_entries_order_is_deterministic() {
    let (_dir, pool) = common::make_test_pool().await;
    let config = common::insert_rule(&pool, "config", "recent-order", "sha-order").await;
    let p20 = insert_source(&pool, config, "daily-math", "p20", "active", 20).await;
    let p10 = insert_source(&pool, config, "daily-math", "p10", "active", 10).await;
    let at = fixed_time(2_000);
    let p20_id = insert_entry(&pool, p20, "p20", at, "pending_fetch").await;
    let p10_first = insert_entry(&pool, p10, "p10-first", at, "pending_fetch").await;
    let p10_second = insert_entry(&pool, p10, "p10-second", at, "pending_fetch").await;

    let rows = recent(&pool, "daily-math", fixed_time(0), 10).await;
    let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();

    assert_eq!(ids, vec![p10_second, p10_first, p20_id]);
}

#[tokio::test]
async fn recent_entries_excludes_dedup_skipped() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = seed_recent_source(&pool, "states", 10).await;
    let at = fixed_time(3_000);
    for state in [
        "pending_fetch",
        "fetching",
        "persisted",
        "failed",
        "dedup_skipped",
    ] {
        insert_entry(&pool, source, state, at, state).await;
    }

    let rows = recent(&pool, "daily-math", fixed_time(0), 10).await;
    let states = rows
        .iter()
        .map(|row| row.state.as_str())
        .collect::<Vec<_>>();

    assert_eq!(states.len(), 4);
    assert!(!states.contains(&"dedup_skipped"));
}

#[tokio::test]
async fn read_only_sqlite_pool_does_not_create_missing_db() {
    let temp = std::env::temp_dir().join(format!(
        "rss-ai-news-read-only-missing-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    std::fs::create_dir_all(&temp).expect("create temp dir");
    let path = temp.join("missing.sqlite");

    let result = build_sqlite_read_only_pool(&path, 1_000).await;

    assert!(result.is_err());
    assert!(
        !path.exists(),
        "read-only open must not create the database"
    );
}

#[tokio::test]
async fn read_only_sqlite_pool_rejects_writes() {
    let (dir, pool) = common::make_test_pool().await;
    let read_only = build_sqlite_read_only_pool(&dir.join("test.sqlite"), 1_000)
        .await
        .expect("open read-only pool");

    let error = sqlx::query("UPDATE rule_versions SET description = 'must-not-write'")
        .execute(&read_only)
        .await
        .expect_err("read-only connection must reject UPDATE");
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("readonly") || message.contains("read-only"),
        "unexpected SQLite error: {error}"
    );

    read_only.close().await;
    pool.close().await;
}

#[tokio::test]
async fn exact_migration_state_rejects_failed_rows_extra_versions_and_checksum_drift() {
    let (_extra_dir, extra_pool) = common::make_test_pool().await;
    let extra_storage = StoragePool::Sqlite(extra_pool.clone());
    ensure_migration_state_exact(&extra_storage)
        .await
        .expect("fresh migrated DB must match embedded state");
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (999999, 'unknown', CURRENT_TIMESTAMP, 1, X'00', 0)",
    )
    .execute(&extra_pool)
    .await
    .expect("insert unknown migration");
    assert!(ensure_migration_state_exact(&extra_storage).await.is_err());

    let (_failed_dir, failed_pool) = common::make_test_pool().await;
    let failed_storage = StoragePool::Sqlite(failed_pool.clone());
    sqlx::query(
        "UPDATE _sqlx_migrations SET success = 0 \
         WHERE version = (SELECT MIN(version) FROM _sqlx_migrations)",
    )
    .execute(&failed_pool)
    .await
    .expect("mark migration failed");
    assert!(ensure_migration_state_exact(&failed_storage).await.is_err());

    let (_checksum_dir, checksum_pool) = common::make_test_pool().await;
    let checksum_storage = StoragePool::Sqlite(checksum_pool.clone());
    sqlx::query(
        "UPDATE _sqlx_migrations SET checksum = X'00' \
         WHERE version = (SELECT MIN(version) FROM _sqlx_migrations)",
    )
    .execute(&checksum_pool)
    .await
    .expect("corrupt migration checksum");
    assert!(
        ensure_migration_state_exact(&checksum_storage)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recent_entries_can_read_while_sqlite_writer_is_active() {
    let (dir, pool) = common::make_test_pool_with_connections(2).await;
    let source = seed_recent_source(&pool, "concurrent", 10).await;
    let id = insert_entry(
        &pool,
        source,
        "concurrent",
        fixed_time(4_000),
        "pending_fetch",
    )
    .await;
    let mut writer = pool.begin().await.expect("begin writer transaction");
    sqlx::query("UPDATE feed_entries SET state = 'fetching' WHERE id = ?")
        .bind(id)
        .execute(&mut *writer)
        .await
        .expect("writer update");

    let read_pool = build_sqlite_read_only_pool(&dir.join("test.sqlite"), 1_000)
        .await
        .expect("open read-only pool");
    let repo = FeedEntryRepo::new_with_storage(StoragePool::Sqlite(read_pool));
    let rows = tokio::time::timeout(
        StdDuration::from_secs(2),
        repo.list_recent(&RecentFeedEntryFilter {
            category_key: "daily-math".to_string(),
            discovered_after: fixed_time(0),
            published_after: None,
            max_rows: 10,
        }),
    )
    .await
    .expect("reader must not block behind active writer")
    .expect("read projection");

    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].state, "pending_fetch",
        "reader sees committed state"
    );
    writer.rollback().await.expect("rollback writer");
    let state: String = sqlx::query_scalar("SELECT state FROM feed_entries WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("state after rollback");
    assert_eq!(state, "pending_fetch");
}

#[tokio::test]
async fn recent_entries_query_plan_uses_existing_indexes() {
    let (_dir, pool) = common::make_test_pool().await;
    let config = common::insert_rule(&pool, "config", "recent-plan", "sha-plan").await;
    let mut sources = Vec::new();
    for (key, priority) in [("plan-a", 10), ("plan-b", 20), ("plan-c", 30)] {
        sources.push(insert_source(&pool, config, "daily-math", key, "active", priority).await);
    }
    let discovered_at = fixed_time(5_000);
    for (index, source) in sources.into_iter().enumerate() {
        seed_many_entries(&pool, source, index, 33_334, discovered_at).await;
    }

    let explain = format!("EXPLAIN QUERY PLAN {RECENT_ENTRIES_SQLITE_QUERY_FOR_DIAGNOSTICS}");
    let plan = sqlx::query_as::<_, (i64, i64, i64, String)>(&explain)
        .bind("daily-math")
        .bind(fixed_time(0) - Duration::days(1))
        .bind(0_i64)
        .bind(0.0_f64)
        .bind(Option::<i64>::None)
        .bind(Option::<f64>::None)
        .bind(201_i64)
        .fetch_all(&pool)
        .await
        .expect("explain query plan")
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        plan.contains("idx_feed_entries_source_discovered_at"),
        "entry source/time index missing from plan:\n{plan}"
    );
    assert!(
        plan.contains("idx_feed_sources_category_priority")
            || plan.contains("idx_feed_sources_status")
            || plan.contains("sqlite_autoindex_feed_sources_1"),
        "source category index missing from plan:\n{plan}"
    );
    let rows = recent(&pool, "daily-math", fixed_time(0), 201).await;
    assert_eq!(rows.len(), 201, "projection must remain bounded");
}

#[tokio::test]
async fn recent_entries_uses_no_feature_specific_schema_objects() {
    let (_dir, pool) = common::make_test_pool().await;

    let objects: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type IN ('table', 'index', 'trigger') AND name LIKE '%recent_entries%'",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect schema objects");

    assert_eq!(
        objects, 0,
        "recent-entries must not introduce schema objects"
    );
}

async fn recent(
    pool: &SqlitePool,
    category_key: &str,
    discovered_after: OffsetDateTime,
    max_rows: u32,
) -> Vec<rss_ai_news_storage::RecentFeedEntry> {
    FeedEntryRepo::new(pool.clone())
        .list_recent(&RecentFeedEntryFilter {
            category_key: category_key.to_string(),
            discovered_after,
            published_after: None,
            max_rows,
        })
        .await
        .expect("list recent entries")
}

async fn seed_recent_source(pool: &SqlitePool, tag: &str, priority: i64) -> i64 {
    let config = common::insert_rule(
        pool,
        "config",
        &format!("recent-{tag}"),
        &format!("sha-recent-{tag}"),
    )
    .await;
    insert_source(pool, config, "daily-math", tag, "active", priority).await
}

async fn insert_source(
    pool: &SqlitePool,
    config_version: i64,
    category: &str,
    key: &str,
    status: &str,
    priority: i64,
) -> i64 {
    sqlx::query_scalar(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind,
            status, priority, config_version
        )
        VALUES (?, ?, ?, ?, 'rss', ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(category)
    .bind(key)
    .bind(key)
    .bind(format!("https://example.com/{key}.xml"))
    .bind(status)
    .bind(priority)
    .bind(config_version)
    .fetch_one(pool)
    .await
    .expect("insert source")
}

async fn insert_entry(
    pool: &SqlitePool,
    source_id: i64,
    title: &str,
    discovered_at: OffsetDateTime,
    state: &str,
) -> i64 {
    sqlx::query_scalar(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            summary_raw, discovered_at, state, dedup_decision
        )
        VALUES (?, ?, ?, ?, ?, 'large-sensitive-summary', ?, ?, 'fresh')
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(format!("uid-{source_id}-{title}"))
    .bind(format!("https://example.com/{source_id}/{title}"))
    .bind(format!("hash-{source_id}-{title}"))
    .bind(title)
    .bind(discovered_at)
    .bind(state)
    .fetch_one(pool)
    .await
    .expect("insert entry")
}

async fn insert_entry_raw_timestamp(
    pool: &SqlitePool,
    source_id: i64,
    title: &str,
    discovered_at: &str,
) -> i64 {
    sqlx::query_scalar(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            discovered_at, state, dedup_decision
        )
        VALUES (?, ?, ?, ?, ?, ?, 'pending_fetch', 'fresh')
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(format!("uid-raw-{source_id}-{title}"))
    .bind(format!("https://example.com/raw/{source_id}/{title}"))
    .bind(format!("hash-raw-{source_id}-{title}"))
    .bind(title)
    .bind(discovered_at)
    .fetch_one(pool)
    .await
    .expect("insert raw timestamp entry")
}

async fn seed_many_entries(
    pool: &SqlitePool,
    source_id: i64,
    source_index: usize,
    count: i64,
    discovered_at: OffsetDateTime,
) {
    sqlx::query(
        r#"
        WITH RECURSIVE counter(value) AS (
            VALUES (1)
            UNION ALL
            SELECT value + 1 FROM counter WHERE value < ?
        )
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            discovered_at, state, dedup_decision
        )
        SELECT ?,
               'bulk-' || ? || '-' || value,
               'https://example.com/bulk/' || ? || '/' || value,
               'bulk-hash-' || ? || '-' || value,
               'bulk title ' || value,
               ?, 'pending_fetch', 'fresh'
        FROM counter
        "#,
    )
    .bind(count)
    .bind(source_id)
    .bind(source_index as i64)
    .bind(source_index as i64)
    .bind(source_index as i64)
    .bind(discovered_at)
    .execute(pool)
    .await
    .expect("bulk insert entries");
}

fn fixed_time(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).expect("valid timestamp")
}
