//! C4 — reindex resume (05-storage §8.1) and per-batch transactions, SQLite.
//!
//! - `claim_resumable_by_target`: an active job of the same rule (kind +
//!   sha256) without a live lease is claimed with its checkpoint; another
//!   rule or a live lease is reported and nothing changes.
//! - `apply_*_batch`: owner check, checkpoint, lease renewal and row writes
//!   commit together; a lost lease rolls the whole batch back.
//!
//! PG equivalents live in `reindex_job_pg_tests.rs`.

mod common;

use rss_ai_news_storage::{
    ReindexBatchCheckpoint, ReindexJobRepo, ReindexJobRepository, ResumeReindexOutcome,
    UpdateContentHashOutcome, UpdateLinkHashOutcome,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{insert_feed_entry, make_test_pool, seed_article, seed_source};

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("valid timestamp")
}

/// A `link_hash` job started for rule sha `sha`, claimed by `owner` until
/// `lease_until`, with checkpoint 42.
async fn crashed_job(repo: &ReindexJobRepo, sha: &str, owner: &str, lease_until: i64) -> i64 {
    let start = repo
        .start_reindex_tx(
            "reindex",
            &format!("tag-{sha}"),
            "d",
            sha,
            "link_hash",
            ts(0),
        )
        .await
        .unwrap();
    repo.claim_by_id(start.job_id, owner, ts(0), ts(lease_until))
        .await
        .unwrap()
        .unwrap();
    assert!(
        repo.advance_checkpoint(start.job_id, owner, 42, ts(1), ts(lease_until))
            .await
            .unwrap()
    );
    start.job_id
}

#[tokio::test]
async fn resumes_expired_job_of_the_same_rule_with_its_checkpoint() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let job_id = crashed_job(&repo, "sha-a", "crashed", 60).await;

    // Running with an expired lease (reclaim not run yet) is resumable too.
    let outcome = repo
        .claim_resumable_by_target("link_hash", "reindex", "sha-a", "rerun", ts(120), ts(720))
        .await
        .unwrap();

    let ResumeReindexOutcome::Resumed(claimed) = outcome else {
        panic!("expected Resumed, got {outcome:?}");
    };
    assert_eq!(claimed.id, job_id);
    assert_eq!(claimed.last_processed_id, Some(42));
    assert_eq!(claimed.attempt_count, 2);
    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.state, "running");
    assert_eq!(job.lease_owner.as_deref(), Some("rerun"));
    assert_eq!(job.lease_expires_at, Some(ts(720)));
}

#[tokio::test]
async fn resumes_reclaimed_pending_job() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let job_id = crashed_job(&repo, "sha-a", "crashed", 60).await;
    assert_eq!(repo.reclaim_expired_leases(ts(120)).await.unwrap(), 1);

    let outcome = repo
        .claim_resumable_by_target("link_hash", "reindex", "sha-a", "rerun", ts(121), ts(721))
        .await
        .unwrap();

    assert!(
        matches!(&outcome, ResumeReindexOutcome::Resumed(c) if c.id == job_id && c.last_processed_id == Some(42)),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_different_rule_is_a_mismatch_and_changes_nothing() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let job_id = crashed_job(&repo, "sha-a", "crashed", 60).await;
    repo.reclaim_expired_leases(ts(120)).await.unwrap();
    let before = repo.find_by_id(job_id).await.unwrap().unwrap();

    let outcome = repo
        .claim_resumable_by_target("link_hash", "reindex", "sha-b", "rerun", ts(121), ts(721))
        .await
        .unwrap();

    assert_eq!(
        outcome,
        ResumeReindexOutcome::RuleMismatch {
            job_id,
            state: "pending".to_string(),
            version_tag: "tag-sha-a".to_string(),
            payload_sha256: "sha-a".to_string(),
        }
    );
    let after = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(
        (after.state, after.attempt_count, after.lease_owner),
        (before.state, before.attempt_count, before.lease_owner)
    );
}

#[tokio::test]
async fn a_live_lease_is_busy_and_is_not_taken_over() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let job_id = crashed_job(&repo, "sha-a", "alive", 600).await;

    let outcome = repo
        .claim_resumable_by_target("link_hash", "reindex", "sha-a", "rerun", ts(120), ts(720))
        .await
        .unwrap();

    assert_eq!(
        outcome,
        ResumeReindexOutcome::Busy {
            job_id,
            state: "running".to_string(),
        }
    );
    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.lease_owner.as_deref(), Some("alive"));
}

#[tokio::test]
async fn no_active_job_and_terminal_jobs_are_ignored() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let job_id = crashed_job(&repo, "sha-a", "crashed", 60).await;
    repo.abort(job_id, "operator", ts(2)).await.unwrap();

    let outcome = repo
        .claim_resumable_by_target("link_hash", "reindex", "sha-a", "rerun", ts(120), ts(720))
        .await
        .unwrap();

    assert_eq!(outcome, ResumeReindexOutcome::NoActiveJob);
}

async fn link_hash_of(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT link_hash FROM feed_entries WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn link_hash_batch_writes_rows_checkpoint_and_lease_together() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let source = seed_source(&pool).await;
    let a = insert_feed_entry(&pool, source, "batch-a", "old-a").await;
    let b = insert_feed_entry(&pool, source, "batch-b", "old-b").await;
    let job_id = crashed_job(&repo, "sha-a", "worker", 600).await;

    let outcomes = repo
        .apply_link_hash_batch(
            ReindexBatchCheckpoint {
                job_id,
                owner: "worker",
                last_processed_id: b,
                now: ts(10),
                lease_expires_at: ts(900),
            },
            &[
                (a, "new-a".to_string()),
                (b, "new-b".to_string()),
                (i64::MAX, "x".to_string()),
            ],
        )
        .await
        .unwrap()
        .expect("lease is held");

    assert_eq!(
        outcomes,
        [
            UpdateLinkHashOutcome::Updated,
            UpdateLinkHashOutcome::Updated,
            UpdateLinkHashOutcome::Missing
        ]
    );
    assert_eq!(link_hash_of(&pool, a).await, "new-a");
    assert_eq!(link_hash_of(&pool, b).await, "new-b");
    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.last_processed_id, Some(b));
    assert_eq!(job.lease_expires_at, Some(ts(900)));
}

#[tokio::test]
async fn link_hash_batch_of_a_worker_that_lost_the_lease_changes_nothing() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let source = seed_source(&pool).await;
    let a = insert_feed_entry(&pool, source, "lost-a", "old-a").await;
    let job_id = crashed_job(&repo, "sha-a", "old-worker", 60).await;
    // Reclaimed and re-claimed by a new worker while the old one still runs.
    repo.reclaim_expired_leases(ts(120)).await.unwrap();
    repo.claim_resumable_by_target(
        "link_hash",
        "reindex",
        "sha-a",
        "new-worker",
        ts(121),
        ts(721),
    )
    .await
    .unwrap();

    let outcome = repo
        .apply_link_hash_batch(
            ReindexBatchCheckpoint {
                job_id,
                owner: "old-worker",
                last_processed_id: a,
                now: ts(122),
                lease_expires_at: ts(722),
            },
            &[(a, "new-a".to_string())],
        )
        .await
        .unwrap();

    assert!(outcome.is_none());
    assert_eq!(link_hash_of(&pool, a).await, "old-a");
    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.last_processed_id, Some(42));
    assert_eq!(job.lease_owner.as_deref(), Some("new-worker"));
}

#[tokio::test]
async fn content_hash_batch_classifies_rows_and_rolls_back_without_lease() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let (article, _, _) = seed_article(&pool).await;
    let current: String = sqlx::query_scalar("SELECT content_hash FROM articles WHERE id = ?")
        .bind(article)
        .fetch_one(&pool)
        .await
        .unwrap();
    let start = repo
        .start_reindex_tx("reindex", "tag-c", "d", "sha-c", "content_hash", ts(0))
        .await
        .unwrap();
    repo.claim_by_id(start.job_id, "worker", ts(0), ts(600))
        .await
        .unwrap()
        .unwrap();
    let checkpoint = |owner| ReindexBatchCheckpoint {
        job_id: start.job_id,
        owner,
        last_processed_id: article,
        now: ts(10),
        lease_expires_at: ts(900),
    };

    let lost = repo
        .apply_content_hash_batch(checkpoint("thief"), &[(article, "fresh".to_string())])
        .await
        .unwrap();
    assert!(lost.is_none());
    let unchanged: String = sqlx::query_scalar("SELECT content_hash FROM articles WHERE id = ?")
        .bind(article)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(unchanged, current);

    let outcomes = repo
        .apply_content_hash_batch(
            checkpoint("worker"),
            &[
                (article, current.clone()),
                (article, "fresh".to_string()),
                (i64::MAX, "missing".to_string()),
            ],
        )
        .await
        .unwrap()
        .expect("lease is held");
    assert_eq!(
        outcomes,
        [
            UpdateContentHashOutcome::Unchanged,
            UpdateContentHashOutcome::Updated,
            UpdateContentHashOutcome::Conflict
        ]
    );
    let job = repo.find_by_id(start.job_id).await.unwrap().unwrap();
    assert_eq!(job.lease_expires_at, Some(ts(900)));
}
