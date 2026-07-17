mod common;

use rss_ai_news_storage::{
    FeedEntryInsertOutcome, FeedEntryRepo, FeedEntryRepository, NewFeedEntry,
};
use sqlx::sqlite::SqlitePoolOptions;
use time::OffsetDateTime;

fn new_entry(source_id: i64, uid: &str, normalized_link: &str, link_hash: &str) -> NewFeedEntry {
    NewFeedEntry {
        source_id,
        feed_entry_uid: uid.to_string(),
        normalized_link: normalized_link.to_string(),
        link_hash: link_hash.to_string(),
        title_raw: format!("title-{uid}"),
        summary_raw: None,
        published_at: None,
        discovered_at: OffsetDateTime::now_utc(),
    }
}

#[tokio::test]
async fn insert_deduplicated_classifies_uid_and_cross_source_link_conflicts() {
    let (_dir, pool) = common::make_test_pool_with_connections(4).await;
    let source_a = common::seed_source(&pool).await;
    let source_b = common::seed_source(&pool).await;
    let repo = FeedEntryRepo::new(pool.clone());

    let first = new_entry(
        source_a,
        "uid-a",
        "https://example.com/shared",
        "shared-hash",
    );
    let link_duplicate = new_entry(
        source_b,
        "uid-b",
        "https://example.com/shared",
        "shared-hash",
    );
    let uid_duplicate = new_entry(
        source_a,
        "uid-a",
        "https://example.com/different",
        "different-hash",
    );

    assert!(matches!(
        repo.insert_deduplicated(&first).await.unwrap(),
        FeedEntryInsertOutcome::Inserted(_)
    ));
    assert_eq!(
        repo.insert_deduplicated(&link_duplicate).await.unwrap(),
        FeedEntryInsertOutcome::LinkDuplicate
    );
    assert_eq!(
        repo.insert_deduplicated(&uid_duplicate).await.unwrap(),
        FeedEntryInsertOutcome::UidDuplicate
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feed_entries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn insert_deduplicated_propagates_non_unique_errors() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = FeedEntryRepo::new(pool);
    let error = repo
        .insert_deduplicated(&new_entry(
            i64::MAX,
            "missing-source",
            "https://example.com/missing-source",
            "missing-source-hash",
        ))
        .await
        .expect_err("foreign-key error must not become LinkDuplicate");
    assert!(
        matches!(error, rss_ai_news_storage::StorageError::Integrity { .. }),
        "unexpected storage error: {error:?}"
    );
}

#[tokio::test]
async fn concurrent_cross_source_link_insert_has_one_canonical_winner() {
    let (_dir, pool) = common::make_test_pool_with_connections(4).await;
    let source_a = common::seed_source(&pool).await;
    let source_b = common::seed_source(&pool).await;
    let repo = FeedEntryRepo::new(pool.clone());
    let entry_a = new_entry(source_a, "race-a", "https://example.com/race", "race-hash");
    let entry_b = new_entry(source_b, "race-b", "https://example.com/race", "race-hash");

    let (left, right) = tokio::join!(
        repo.insert_deduplicated(&entry_a),
        repo.insert_deduplicated(&entry_b)
    );
    let outcomes = [left.unwrap(), right.unwrap()];

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, FeedEntryInsertOutcome::Inserted(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, FeedEntryInsertOutcome::LinkDuplicate))
            .count(),
        1
    );

    let canonical_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feed_entries WHERE link_hash = ? AND link_dedup_shadow = 0",
    )
    .bind("race-hash")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(canonical_count, 1);
}

#[tokio::test]
async fn migration_0004_preserves_duplicates_and_marks_deterministic_shadow() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    for migration in [
        include_str!("../../../migrations/sqlite/0001_init.up.sql"),
        include_str!("../../../migrations/sqlite/0002_reindex_jobs_and_rule_status.up.sql"),
        include_str!("../../../migrations/sqlite/0003_ai_effective_model_id.up.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }

    let config_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) VALUES ('config', 'legacy', 'legacy', 'legacy-sha') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let source_a: i64 = sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, config_version) VALUES ('ai', 'legacy-a', 'A', 'https://example.com/a.xml', 'rss', ?) RETURNING id",
    )
    .bind(config_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let source_b: i64 = sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, config_version) VALUES ('ai', 'legacy-b', 'B', 'https://example.com/b.xml', 'rss', ?) RETURNING id",
    )
    .bind(config_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    for (source_id, uid) in [(source_a, "legacy-a"), (source_b, "legacy-b")] {
        sqlx::query(
            "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, title_raw, discovered_at, state, dedup_decision) VALUES (?, ?, 'https://example.com/legacy', 'legacy-shared', 'legacy', ?, 'pending_fetch', 'fresh')",
        )
        .bind(source_id)
        .bind(uid)
        .bind(OffsetDateTime::now_utc())
        .execute(&pool)
        .await
        .unwrap();
    }

    sqlx::raw_sql(include_str!(
        "../../../migrations/sqlite/0004_feed_entry_link_dedup_atomicity.up.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT id, link_dedup_shadow FROM feed_entries WHERE link_hash = 'legacy-shared' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, 0, "lowest id must remain canonical");
    assert_eq!(rows[1].1, 1, "later duplicate must be preserved as shadow");

    let third = sqlx::query(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, title_raw, discovered_at, state, dedup_decision) VALUES (?, 'legacy-c', 'https://example.com/legacy', 'legacy-shared', 'legacy', ?, 'pending_fetch', 'fresh')",
    )
    .bind(source_b)
    .bind(OffsetDateTime::now_utc())
    .execute(&pool)
    .await;
    assert!(
        third.is_err(),
        "partial unique index must reject a second canonical row"
    );
}
