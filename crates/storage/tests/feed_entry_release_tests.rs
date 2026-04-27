mod common;

use rss_ai_news_storage::{
    ClaimRequest, FeedEntryRepository, SqliteFeedEntryRepo, lease_expires_at,
};
use time::{Duration, OffsetDateTime};

use common::{insert_article, insert_feed_entry, insert_rule, make_test_pool, seed_source};

#[tokio::test]
async fn release_dedup_skipped_sets_state_and_article_id() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, entry_id, article_id) = setup_claimed_entry(&pool, "worker-a").await;

    let released = repo
        .release_dedup_skipped(
            entry_id,
            "worker-a",
            article_id,
            "hash_dup",
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");
    let row = state_row(&pool, entry_id).await;

    assert!(released);
    assert_eq!(row.0, "dedup_skipped");
    assert_eq!(row.1, Some(article_id));
    assert_eq!(row.2, Some("hash_dup".to_string()));
    assert!(row.3.is_none());
}

#[tokio::test]
async fn release_dedup_skipped_with_wrong_owner_returns_false() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, entry_id, article_id) = setup_claimed_entry(&pool, "worker-a").await;

    let released = repo
        .release_dedup_skipped(
            entry_id,
            "worker-b",
            article_id,
            "hash_dup",
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("wrong owner release should not error");
    let row = state_row(&pool, entry_id).await;

    assert!(!released);
    assert_eq!(row.0, "fetching");
    assert_eq!(row.1, None);
}

#[tokio::test]
async fn release_fallback_persisted_sets_state_and_article_id() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, entry_id, article_id) = setup_claimed_entry(&pool, "worker-a").await;

    let released = repo
        .release_fallback_persisted(entry_id, "worker-a", article_id, OffsetDateTime::now_utc())
        .await
        .expect("release should succeed");
    let row = state_row(&pool, entry_id).await;

    assert!(released);
    assert_eq!(row.0, "fallback_persisted");
    assert_eq!(row.1, Some(article_id));
    assert!(row.3.is_none());
}

#[tokio::test]
async fn release_fallback_persisted_with_wrong_owner_returns_false() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, entry_id, article_id) = setup_claimed_entry(&pool, "worker-a").await;

    let released = repo
        .release_fallback_persisted(entry_id, "worker-b", article_id, OffsetDateTime::now_utc())
        .await
        .expect("wrong owner release should not error");
    let row = state_row(&pool, entry_id).await;

    assert!(!released);
    assert_eq!(row.0, "fetching");
    assert_eq!(row.1, None);
}

async fn setup_claimed_entry(
    pool: &sqlx::SqlitePool,
    owner: &str,
) -> (SqliteFeedEntryRepo, i64, i64) {
    let source_id = seed_source(pool).await;
    let entry_id = insert_feed_entry(pool, source_id, "uid-1", "link-hash-1").await;
    let rule_id = insert_rule(pool, "extractor", "release-test", "release-test-sha").await;
    let other_entry_id =
        insert_feed_entry(pool, source_id, "uid-article", "link-hash-article").await;
    let article_id = insert_article(pool, "content-hash-release", other_entry_id, rule_id)
        .await
        .expect("article should insert");
    sqlx::query("UPDATE feed_entries SET state = 'persisted', article_id = ? WHERE id = ?")
        .bind(article_id)
        .bind(other_entry_id)
        .execute(pool)
        .await
        .expect("article origin entry should not be claimable");
    let repo = SqliteFeedEntryRepo::new(pool.clone());
    let claimed = repo
        .claim_pending_fetch(&claim_request(owner))
        .await
        .expect("claim should succeed");
    assert!(claimed.iter().any(|entry| entry.id == entry_id));
    (repo, entry_id, article_id)
}

async fn state_row(
    pool: &sqlx::SqlitePool,
    entry_id: i64,
) -> (String, Option<i64>, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT state, article_id, dedup_decision, lease_owner FROM feed_entries WHERE id = ?",
    )
    .bind(entry_id)
    .fetch_one(pool)
    .await
    .expect("entry should be readable")
}

fn claim_request(owner: &str) -> ClaimRequest {
    let now = OffsetDateTime::now_utc();
    ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires_at(now, Duration::seconds(60)),
        batch_size: 1,
        max_attempts: 5,
    }
}
