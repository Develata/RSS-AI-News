//! A10: the extract claim only takes entries of active sources, optionally
//! limited to one category (`ingest --category`, plan/09 §2). SQLite; the PG
//! mirror is `pg_claim_is_scoped_to_category_and_active_sources`.

mod common;

use rss_ai_news_storage::{ClaimRequest, FeedEntryRepo, FeedEntryRepository, lease_expires_at};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_feed_entry, insert_rule, make_test_pool};

async fn source(pool: &SqlitePool, config_id: i64, category: &str, key: &str, status: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, \
             status, config_version) \
         VALUES (?, ?, 'S', 'https://example.com/feed.xml', 'rss', ?, ?) RETURNING id",
    )
    .bind(category)
    .bind(key)
    .bind(status)
    .bind(config_id)
    .fetch_one(pool)
    .await
    .expect("source should insert")
}

fn request() -> ClaimRequest {
    let now = OffsetDateTime::now_utc();
    ClaimRequest {
        owner: "worker".to_string(),
        now,
        lease_expires_at: lease_expires_at(now, Duration::minutes(5)),
        batch_size: 10,
        max_attempts: 5,
    }
}

async fn claimed_ids(repo: &FeedEntryRepo, category: Option<&str>) -> Vec<i64> {
    let mut ids: Vec<i64> = repo
        .claim_pending_fetch_in_scope(&request(), &[], category)
        .await
        .expect("claim")
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn claim_is_scoped_to_category_and_active_sources() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_rule(&pool, "config", "cfg", "cfg-sha").await;
    let a = source(&pool, config_id, "A", "a", "active").await;
    let a_paused = source(&pool, config_id, "A", "a-paused", "paused").await;
    let b = source(&pool, config_id, "B", "b", "active").await;
    let in_a = insert_feed_entry(&pool, a, "a-1", "h-a-1").await;
    let in_a_paused = insert_feed_entry(&pool, a_paused, "ap-1", "h-ap-1").await;
    let in_b = insert_feed_entry(&pool, b, "b-1", "h-b-1").await;
    let repo = FeedEntryRepo::new(pool.clone());

    assert_eq!(claimed_ids(&repo, Some("A")).await, vec![in_a]);
    let state_b: String = sqlx::query_scalar("SELECT state FROM feed_entries WHERE id = ?")
        .bind(in_b)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state_b, "pending_fetch", "category B backlog untouched");

    // Unscoped: every active source, still never the paused one.
    assert_eq!(claimed_ids(&repo, None).await, vec![in_b]);
    assert!(claimed_ids(&repo, None).await.is_empty());
    let state_paused: String = sqlx::query_scalar("SELECT state FROM feed_entries WHERE id = ?")
        .bind(in_a_paused)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state_paused, "pending_fetch");
}

#[tokio::test]
async fn unscoped_claim_method_keeps_its_contract() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_rule(&pool, "config", "cfg", "cfg-sha").await;
    let a = source(&pool, config_id, "A", "a", "active").await;
    let archived = source(&pool, config_id, "A", "gone", "archived").await;
    let live = insert_feed_entry(&pool, a, "a-1", "h-a-1").await;
    insert_feed_entry(&pool, archived, "g-1", "h-g-1").await;
    let repo = FeedEntryRepo::new(pool.clone());

    let claimed = repo.claim_pending_fetch(&request(), &[]).await.unwrap();
    assert_eq!(
        claimed.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        [live]
    );
}
