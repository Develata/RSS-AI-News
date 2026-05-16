mod common;

use std::{collections::HashSet, sync::Arc};

use rss_ai_news_storage::{ClaimRequest, FeedEntryRepo, FeedEntryRepository, lease_expires_at};
use time::{Duration, OffsetDateTime};

use common::{insert_feed_entry, make_test_pool_with_connections, seed_source};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_claim_returns_disjoint_rows() {
    let (_dir, pool) = make_test_pool_with_connections(4).await;
    let source_id = seed_source(&pool).await;
    for index in 0..100 {
        insert_feed_entry(
            &pool,
            source_id,
            &format!("uid-{index}"),
            &format!("link-hash-{index}"),
        )
        .await;
    }
    let repo = Arc::new(FeedEntryRepo::new(pool));

    let mut handles = Vec::new();
    for worker in 0..4 {
        let repo = Arc::clone(&repo);
        handles.push(tokio::spawn(async move {
            let now = OffsetDateTime::now_utc();
            repo.claim_pending_fetch(&ClaimRequest {
                owner: format!("worker-{worker}"),
                now,
                lease_expires_at: lease_expires_at(now, Duration::seconds(60)),
                batch_size: 30,
                max_attempts: 5,
            })
            .await
        }));
    }

    let mut all_ids = Vec::new();
    for handle in handles {
        let claimed = handle
            .await
            .expect("worker should join")
            .expect("claim should succeed");
        all_ids.extend(claimed.into_iter().map(|row| row.id));
    }

    let unique = all_ids.iter().copied().collect::<HashSet<_>>();
    assert_eq!(unique.len(), all_ids.len(), "ids should be unique");
    assert!(all_ids.len() <= 100);
    assert!(
        all_ids.len() >= 30,
        "at least one worker should get a full batch"
    );
}

#[tokio::test]
async fn release_with_wrong_owner_returns_false() {
    let (_dir, pool) = make_test_pool_with_connections(2).await;
    let source_id = seed_source(&pool).await;
    let entry_id = insert_feed_entry(&pool, source_id, "uid-1", "link-hash-1").await;
    let repo = FeedEntryRepo::new(pool);

    let claimed = repo
        .claim_pending_fetch(&claim_request("worker-a", 1))
        .await
        .expect("claim should succeed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, entry_id);

    let released = repo
        .release_success(entry_id, "worker-b", 1, OffsetDateTime::now_utc())
        .await
        .expect("wrong owner release should not error");
    assert!(!released, "release with wrong owner must return false");
}

#[tokio::test]
async fn reclaim_expired_lease_clears_owner_and_allows_reclaim() {
    let (_dir, pool) = make_test_pool_with_connections(2).await;
    let source_id = seed_source(&pool).await;
    let entry_id = insert_feed_entry(&pool, source_id, "uid-1", "link-hash-1").await;
    let repo = FeedEntryRepo::new(pool.clone());

    let claimed = repo
        .claim_pending_fetch(&claim_request("worker-a", 1))
        .await
        .expect("claim should succeed");
    assert_eq!(claimed.len(), 1);

    let past = OffsetDateTime::now_utc() - Duration::seconds(1);
    sqlx::query("UPDATE feed_entries SET lease_expires_at = ? WHERE id = ?")
        .bind(past)
        .bind(entry_id)
        .execute(&pool)
        .await
        .expect("lease should be made expired");

    let reclaimed = repo
        .reclaim_expired_leases(OffsetDateTime::now_utc())
        .await
        .expect("reclaim should succeed");
    assert_eq!(reclaimed, 1);

    let (state, owner): (String, Option<String>) =
        sqlx::query_as("SELECT state, lease_owner FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("entry should be readable");
    assert_eq!(state, "pending_fetch");
    assert!(owner.is_none());

    let claimed_again = repo
        .claim_pending_fetch(&claim_request("worker-b", 1))
        .await
        .expect("second claim should succeed");
    assert_eq!(claimed_again.len(), 1);
    assert_eq!(claimed_again[0].id, entry_id);
}

fn claim_request(owner: &str, batch_size: u32) -> ClaimRequest {
    let now = OffsetDateTime::now_utc();
    ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires_at(now, Duration::seconds(60)),
        batch_size,
        max_attempts: 5,
    }
}
