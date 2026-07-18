mod common;

use std::sync::Arc;

use rss_ai_news_runtime::{
    MAX_RECENT_ENTRIES_LIMIT, MAX_RECENT_SOURCE_HEALTH_ROWS, RecentEntriesFlow,
    RecentEntriesOptions, RuntimeError,
};
use rss_ai_news_storage::{FeedEntryRepo, FeedSourceRepo};
use sqlx::SqlitePool;
use time::OffsetDateTime;

#[tokio::test]
async fn recent_entries_limit_plus_one_sets_truncated() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = seed_source(&pool).await;
    for (index, discovered_at) in [10_i64, 20, 30].into_iter().enumerate() {
        insert_entry(&pool, source, index, fixed_time(discovered_at)).await;
    }
    let flow = RecentEntriesFlow::new(
        Arc::new(FeedSourceRepo::new(pool.clone())),
        Arc::new(FeedEntryRepo::new(pool)),
    );

    let result = flow
        .execute(RecentEntriesOptions {
            category_key: "daily-math".to_string(),
            discovered_after: fixed_time(0),
            published_after: None,
            limit: 2,
        })
        .await
        .expect("recent entries flow");

    assert!(result.truncated);
    assert_eq!(result.entries.len(), 2);
    assert_eq!(result.entries[0].title, "title-2");
    assert_eq!(result.entries[1].title, "title-1");
    assert_eq!(result.source_health.len(), 1);
    assert_eq!(result.source_health[0].source_key, "flow-source");
}

#[tokio::test]
async fn recent_entries_source_health_is_bounded_and_truncated() {
    let (_dir, pool) = common::make_test_pool().await;
    let config = common::insert_config_rule(&pool).await;
    sqlx::query(
        r#"
        WITH RECURSIVE source_no(value) AS (
            SELECT 1
            UNION ALL
            SELECT value + 1 FROM source_no WHERE value < 501
        )
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind,
            status, priority, config_version, last_error_kind
        )
        SELECT 'daily-math',
               CASE WHEN value = 1
                    THEN printf('source-%0300d', value)
                    ELSE printf('source-%04d', value)
               END,
               printf('Source %04d', value),
               printf('https://example.test/%04d.xml', value),
               'rss', 'active', value, ?,
               CASE WHEN value = 1 THEN replace(hex(zeroblob(200)), '00', 'e') ELSE NULL END
        FROM source_no
        "#,
    )
    .bind(config)
    .execute(&pool)
    .await
    .expect("seed 501 sources");
    let flow = RecentEntriesFlow::new(
        Arc::new(FeedSourceRepo::new(pool.clone())),
        Arc::new(FeedEntryRepo::new(pool)),
    );

    let summary = flow
        .execute(RecentEntriesOptions {
            category_key: "daily-math".to_string(),
            discovered_after: fixed_time(0),
            published_after: None,
            limit: 1,
        })
        .await
        .expect("execute bounded source health projection");

    assert_eq!(
        summary.source_health.len(),
        MAX_RECENT_SOURCE_HEALTH_ROWS as usize
    );
    assert!(summary.source_health_truncated);
    assert_eq!(summary.source_health[0].source_key.chars().count(), 256);
    assert!(
        summary
            .source_health
            .iter()
            .all(|source| source.source_key.chars().count() <= 256)
    );
    assert_eq!(
        summary.source_health[0]
            .last_error_kind
            .as_deref()
            .map(str::len),
        Some(128)
    );
}

#[tokio::test]
async fn recent_entries_flow_rejects_out_of_range_limit() {
    let (_dir, pool) = common::make_test_pool().await;
    let flow = RecentEntriesFlow::new(
        Arc::new(FeedSourceRepo::new(pool.clone())),
        Arc::new(FeedEntryRepo::new(pool)),
    );

    for limit in [0, MAX_RECENT_ENTRIES_LIMIT + 1] {
        let error = flow
            .execute(RecentEntriesOptions {
                category_key: "daily-math".to_string(),
                discovered_after: fixed_time(0),
                published_after: None,
                limit,
            })
            .await
            .expect_err("invalid limit should fail");
        assert!(matches!(error, RuntimeError::Config(_)));
    }
}

async fn seed_source(pool: &SqlitePool) -> i64 {
    let config = common::insert_config_rule(pool).await;
    sqlx::query_scalar(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind,
            status, priority, consecutive_failures, last_error_kind, config_version
        )
        VALUES ('daily-math', 'flow-source', 'Flow Source',
                'https://example.com/feed.xml', 'rss', 'active', 10, 2,
                'http_timeout', ?)
        RETURNING id
        "#,
    )
    .bind(config)
    .fetch_one(pool)
    .await
    .expect("insert source")
}

async fn insert_entry(
    pool: &SqlitePool,
    source_id: i64,
    index: usize,
    discovered_at: OffsetDateTime,
) {
    sqlx::query(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            summary_raw, discovered_at, state, dedup_decision
        )
        VALUES (?, ?, ?, ?, ?, 'must-not-be-projected', ?, 'pending_fetch', 'fresh')
        "#,
    )
    .bind(source_id)
    .bind(format!("uid-{index}"))
    .bind(format!("https://example.com/{index}"))
    .bind(format!("hash-{index}"))
    .bind(format!("title-{index}"))
    .bind(discovered_at)
    .execute(pool)
    .await
    .expect("insert entry");
}

fn fixed_time(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).expect("valid timestamp")
}
