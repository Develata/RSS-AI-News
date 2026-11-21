//! R2-S14 — reindex_jobs lease comparisons on SQLite are by instant, not by
//! text (same premise as `ai_decision_lease_instant_tests.rs`): within one
//! second `…00.5Z` sorts before `…00Z` as text. Resume, reclaim and the two
//! claims must not take over a lease that has not expired.

mod common;

use rss_ai_news_storage::{ReindexJobRepo, ReindexJobRepository, ResumeReindexOutcome};
use time::{Duration, OffsetDateTime};

use common::make_test_pool;

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("valid timestamp")
}

/// A running `link_hash` job leased by `crashed` until `lease_until`.
async fn running_job(repo: &ReindexJobRepo, lease_until: OffsetDateTime) -> i64 {
    let start = repo
        .start_reindex_tx("reindex", "tag", "d", "sha-a", "link_hash", ts(0))
        .await
        .unwrap();
    repo.claim_by_id(start.job_id, "crashed", ts(0), lease_until)
        .await
        .unwrap()
        .unwrap();
    start.job_id
}

#[tokio::test]
async fn resume_does_not_take_over_a_lease_that_expires_later_in_the_same_second() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool);
    let job_id = running_job(&repo, ts(60) + Duration::milliseconds(500)).await;

    let outcome = repo
        .claim_resumable_by_target("link_hash", "reindex", "sha-a", "rerun", ts(60), ts(600))
        .await
        .unwrap();

    assert!(
        matches!(outcome, ResumeReindexOutcome::Busy { .. }),
        "{outcome:?}"
    );
    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.lease_owner.as_deref(), Some("crashed"));
    assert_eq!(job.attempt_count, 1);
}

#[tokio::test]
async fn reclaim_is_strict_and_by_instant() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool);
    running_job(&repo, ts(60) + Duration::milliseconds(500)).await;

    // Same second, earlier instant; then the exact deadline: not expired.
    assert_eq!(repo.reclaim_expired_leases(ts(60)).await.unwrap(), 0);
    assert_eq!(
        repo.reclaim_expired_leases(ts(60) + Duration::milliseconds(500))
            .await
            .unwrap(),
        0
    );
    // `…00.51Z` sorts before `…00.5Z` as text but is later.
    assert_eq!(
        repo.reclaim_expired_leases(ts(60) + Duration::milliseconds(510))
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn pending_claims_respect_a_live_lease_by_instant() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let start = repo
        .start_reindex_tx("reindex", "tag", "d", "sha-a", "link_hash", ts(0))
        .await
        .unwrap();
    // A pending row that still carries a live lease (fractional deadline).
    sqlx::query("UPDATE reindex_jobs SET lease_expires_at = ? WHERE id = ?")
        .bind(ts(60) + Duration::milliseconds(500))
        .bind(start.job_id)
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        repo.claim_pending("w", ts(60), ts(600))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.claim_by_id(start.job_id, "w", ts(60), ts(600))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repo.claim_by_id(start.job_id, "w", ts(61), ts(600))
            .await
            .unwrap()
            .is_some()
    );
}
