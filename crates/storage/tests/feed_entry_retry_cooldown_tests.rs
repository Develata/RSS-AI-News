//! Extract retry cooldown (`feed_entries.next_attempt_at`, migration 0007) on
//! SQLite. The PostgreSQL mirror lives in `feed_entry_pg_tests.rs`.
//!
//! All instants are explicit whole-second timestamps; nothing sleeps.
//! - A retryable release writes the deadline in the same owner-guarded UPDATE
//!   that drops the lease; a wrong owner changes nothing.
//! - The claim skips rows whose deadline is after the cutoff, before `LIMIT`,
//!   so cooling rows take no batch slot. The deadline itself is inclusive.
//! - The cutoff, not the claim's `now`, decides eligibility: with the cutoff at
//!   run start, a deadline that passes mid-run still does not make the row
//!   claimable in that run.
//! - Exhaustion sweep and backfill reset are independent of the cooldown.

mod common;

use rss_ai_news_storage::{
    ClaimRequest, FeedEntryRepo, FeedEntryRepository, ResetFailedFilter, lease_expires_at,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_feed_entry, seed_source};

const MAX_ATTEMPTS: u32 = 5;
/// Fixed run start (2026-11-20T10:20:00Z); every other instant is an offset
/// from it.
fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1795170000).expect("valid timestamp")
}

fn claim(owner: &str, now: OffsetDateTime, batch_size: u32) -> ClaimRequest {
    ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires_at(now, Duration::minutes(5)),
        batch_size,
        max_attempts: MAX_ATTEMPTS,
    }
}

async fn row(pool: &SqlitePool, id: i64) -> (String, Option<OffsetDateTime>, i64, Option<String>) {
    sqlx::query_as(
        "SELECT state, next_attempt_at, attempt_count, lease_owner FROM feed_entries WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("entry should be readable")
}

async fn set_cooldown(pool: &SqlitePool, id: i64, at: OffsetDateTime) {
    sqlx::query("UPDATE feed_entries SET next_attempt_at = ? WHERE id = ?")
        .bind(at)
        .bind(id)
        .execute(pool)
        .await
        .expect("cooldown should be set");
}

#[tokio::test]
async fn retryable_release_sets_deadline_and_claim_honours_it() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let id = insert_feed_entry(&pool, source_id, "uid-cool", "hash-cool").await;
    let repo = FeedEntryRepo::new(pool.clone());

    let claimed = repo
        .claim_pending_fetch(&claim("worker-a", t0(), 1), t0())
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let deadline = t0() + Duration::minutes(10);
    let outcome = repo
        .release_retryable_failure(
            id,
            "worker-a",
            "timeout",
            "http_timeout",
            MAX_ATTEMPTS,
            deadline,
            t0() + Duration::seconds(30),
        )
        .await
        .unwrap();
    assert!(outcome.released && !outcome.exhausted);
    assert_eq!(
        row(&pool, id).await,
        ("pending_fetch".to_string(), Some(deadline), 1, None)
    );

    // Same run: cutoff stays at run start even though `now` is past the
    // deadline (the cooldown expired mid-run). The row must stay unclaimed.
    let late = t0() + Duration::minutes(20);
    assert!(
        repo.claim_pending_fetch(&claim("worker-a", late, 1), t0())
            .await
            .unwrap()
            .is_empty()
    );
    // Later run that starts one second before the deadline: still cooling.
    let just_before = deadline - Duration::seconds(1);
    assert!(
        repo.claim_pending_fetch(&claim("worker-b", just_before, 1), just_before)
            .await
            .unwrap()
            .is_empty()
    );
    // Run that starts exactly at the deadline: eligible again.
    let retried = repo
        .claim_pending_fetch(&claim("worker-c", deadline, 1), deadline)
        .await
        .unwrap();
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0].id, id);
    assert_eq!(retried[0].attempt_count, 2);
}

#[tokio::test]
async fn cooling_entry_takes_no_batch_slot() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    // Discovered first, so oldest-first ordering would pick it without the filter.
    let cooling = insert_feed_entry(&pool, source_id, "uid-old", "hash-old").await;
    let fresh = insert_feed_entry(&pool, source_id, "uid-new", "hash-new").await;
    set_cooldown(&pool, cooling, t0() + Duration::hours(1)).await;
    let repo = FeedEntryRepo::new(pool.clone());

    let claimed = repo
        .claim_pending_fetch(&claim("worker-a", t0(), 1), t0())
        .await
        .unwrap();
    assert_eq!(claimed.iter().map(|e| e.id).collect::<Vec<_>>(), [fresh]);
    // Only cooling rows left: the claim is empty, not a spin.
    assert!(
        repo.claim_pending_fetch(&claim("worker-a", t0(), 1), t0())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(row(&pool, cooling).await.2, 0, "cooling row never claimed");
}

#[tokio::test]
async fn wrong_owner_release_writes_no_deadline() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let id = insert_feed_entry(&pool, source_id, "uid-owner", "hash-owner").await;
    let repo = FeedEntryRepo::new(pool.clone());
    repo.claim_pending_fetch(&claim("worker-a", t0(), 1), t0())
        .await
        .unwrap();

    let outcome = repo
        .release_retryable_failure(
            id,
            "worker-b",
            "timeout",
            "http_timeout",
            MAX_ATTEMPTS,
            t0() + Duration::minutes(10),
            t0(),
        )
        .await
        .unwrap();
    assert!(!outcome.released && !outcome.exhausted);
    assert_eq!(
        row(&pool, id).await,
        (
            "fetching".to_string(),
            None,
            1,
            Some("worker-a".to_string())
        )
    );
}

#[tokio::test]
async fn exhaustion_stays_terminal_regardless_of_cooldown() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let repo = FeedEntryRepo::new(pool.clone());

    // Final attempt: the release folds to `failed`; the deadline it also writes
    // is inert because no claim reads `failed` rows.
    let last = insert_feed_entry(&pool, source_id, "uid-last", "hash-last").await;
    sqlx::query("UPDATE feed_entries SET attempt_count = ? WHERE id = ?")
        .bind(i64::from(MAX_ATTEMPTS - 1))
        .bind(last)
        .execute(&pool)
        .await
        .unwrap();
    repo.claim_pending_fetch(&claim("worker-a", t0(), 1), t0())
        .await
        .unwrap();
    let outcome = repo
        .release_retryable_failure(
            last,
            "worker-a",
            "timeout",
            "http_timeout",
            MAX_ATTEMPTS,
            t0() + Duration::hours(2),
            t0(),
        )
        .await
        .unwrap();
    assert!(outcome.released && outcome.exhausted);
    assert_eq!(row(&pool, last).await.0, "failed");

    // Sweep: an exhausted pending_fetch row still cooling is terminalized.
    let swept = insert_feed_entry(&pool, source_id, "uid-swept", "hash-swept").await;
    sqlx::query("UPDATE feed_entries SET attempt_count = ? WHERE id = ?")
        .bind(i64::from(MAX_ATTEMPTS))
        .bind(swept)
        .execute(&pool)
        .await
        .unwrap();
    set_cooldown(&pool, swept, t0() + Duration::hours(1)).await;
    assert_eq!(
        repo.terminalize_exhausted(MAX_ATTEMPTS, t0())
            .await
            .unwrap(),
        1
    );
    assert_eq!(row(&pool, swept).await.0, "failed");
}

#[tokio::test]
async fn backfill_reset_clears_cooldown() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let id = insert_feed_entry(&pool, source_id, "uid-reset", "hash-reset").await;
    sqlx::query("UPDATE feed_entries SET state = 'failed', attempt_count = 5 WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    set_cooldown(&pool, id, t0() + Duration::hours(2)).await;
    let repo = FeedEntryRepo::new(pool.clone());

    let outcome = repo
        .reset_failed_in_window(&ResetFailedFilter::default())
        .await
        .unwrap();
    assert_eq!(outcome.reset, 1);
    assert_eq!(
        row(&pool, id).await,
        ("pending_fetch".to_string(), None, 0, None)
    );
    assert_eq!(
        repo.claim_pending_fetch(&claim("worker-a", t0(), 1), t0())
            .await
            .unwrap()
            .len(),
        1
    );
}
