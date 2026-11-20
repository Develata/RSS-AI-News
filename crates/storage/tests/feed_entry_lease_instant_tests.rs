//! Lease-expiry comparisons on `feed_entries` (SQLite) are by instant, not by
//! text.
//!
//! sqlx stores `OffsetDateTime` as RFC3339 TEXT with a trimmed, variable-width
//! fraction, so within one second text order differs from instant order
//! (`…00.12Z` < `…00.1Z`, `…00Z` > `…00.5Z`). The claim, the reclaim and the
//! exhaustion sweep all test `lease_expires_at < now` and must agree with the
//! instants in both directions, keep the strict `<`, and never treat a lease as
//! expired early (sub-millisecond gaps included).
//!
//! All instants are explicit; nothing sleeps. Rows are seeded with raw SQL
//! because the repo never leaves a `pending_fetch` row with a lease.

mod common;

use rss_ai_news_storage::{ClaimRequest, FeedEntryRepo, FeedEntryRepository};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime, UtcOffset};

use common::{insert_feed_entry, seed_source};

const MAX_ATTEMPTS: u32 = 5;

/// 2026-11-20T10:20:00Z, a whole second.
fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1795170000).expect("valid timestamp")
}

fn ms(n: i64) -> Duration {
    Duration::milliseconds(n)
}

fn claim_at(now: OffsetDateTime) -> ClaimRequest {
    ClaimRequest {
        owner: "worker-new".to_string(),
        now,
        lease_expires_at: now + Duration::minutes(5),
        batch_size: 10,
        max_attempts: MAX_ATTEMPTS,
    }
}

/// Puts `id` in `state` held by `worker-old` until `lease`, bound through sqlx
/// exactly as production writes it.
async fn set_lease(pool: &SqlitePool, id: i64, state: &str, attempts: u32, lease: OffsetDateTime) {
    sqlx::query(
        "UPDATE feed_entries
         SET state = ?, attempt_count = ?, lease_owner = 'worker-old', lease_expires_at = ?
         WHERE id = ?",
    )
    .bind(state)
    .bind(i64::from(attempts))
    .bind(lease)
    .bind(id)
    .execute(pool)
    .await
    .expect("lease should be set");
}

async fn lease_text(pool: &SqlitePool, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT CAST(lease_expires_at AS TEXT) FROM feed_entries WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("entry should be readable")
}

async fn state_and_owner(pool: &SqlitePool, id: i64) -> (String, Option<String>) {
    sqlx::query_as("SELECT state, lease_owner FROM feed_entries WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("entry should be readable")
}

/// Which predicate a case exercises.
#[derive(Clone, Copy, Debug)]
enum Op {
    Claim,
    Reclaim,
    Terminalize,
}

const OPS: [Op; 3] = [Op::Claim, Op::Reclaim, Op::Terminalize];

/// Seeds one leased row for `op`, runs `op` at `now`, and reports whether the
/// lease was treated as expired.
async fn expired(op: Op, lease: OffsetDateTime, now: OffsetDateTime) -> bool {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let id = insert_feed_entry(&pool, source_id, "uid-lease", "hash-lease").await;
    let (state, attempts) = match op {
        Op::Claim => ("pending_fetch", 0),
        Op::Reclaim => ("fetching", 1),
        Op::Terminalize => ("pending_fetch", MAX_ATTEMPTS),
    };
    set_lease(&pool, id, state, attempts, lease).await;
    let repo = FeedEntryRepo::new(pool.clone());

    let hit = match op {
        Op::Claim => {
            let claimed = repo.claim_pending_fetch(&claim_at(now), &[]).await.unwrap();
            claimed.iter().any(|e| e.id == id)
        }
        Op::Reclaim => repo.reclaim_expired_leases(now).await.unwrap() == 1,
        Op::Terminalize => repo.terminalize_exhausted(MAX_ATTEMPTS, now).await.unwrap() == 1,
    };
    let (state_after, owner_after) = state_and_owner(&pool, id).await;
    let expected_state = match (op, hit) {
        (_, false) => state,
        (Op::Claim, true) => "fetching",
        (Op::Reclaim, true) => "pending_fetch",
        (Op::Terminalize, true) => "failed",
    };
    assert_eq!(state_after, expected_state, "{op:?}: state after the call");
    let expected_owner = match (op, hit) {
        (_, false) => Some("worker-old"),
        (Op::Claim, true) => Some("worker-new"),
        (_, true) => None,
    };
    assert_eq!(
        owner_after.as_deref(),
        expected_owner,
        "{op:?}: owner after the call"
    );
    hit
}

#[tokio::test]
async fn stored_lease_text_has_variable_width_fraction() {
    // Guards the premise of this file: if sqlx ever switched to a fixed-width
    // encoding, the cases below would stop exercising the bug.
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let id = insert_feed_entry(&pool, source_id, "uid-text", "hash-text").await;
    set_lease(&pool, id, "fetching", 1, t0() + ms(120)).await;
    assert_eq!(
        lease_text(&pool, id).await.as_deref(),
        Some("2026-11-20T10:20:00.12Z")
    );
    set_lease(&pool, id, "fetching", 1, t0()).await;
    assert_eq!(
        lease_text(&pool, id).await.as_deref(),
        Some("2026-11-20T10:20:00Z")
    );
}

#[tokio::test]
async fn lease_later_in_the_same_second_is_not_expired() {
    // Text: "…00.12Z" < "…00.1Z", yet the lease ends 20 ms after now.
    for op in OPS {
        assert!(
            !expired(op, t0() + ms(120), t0() + ms(100)).await,
            "{op:?}: a valid lease must not be taken over early"
        );
    }
}

#[tokio::test]
async fn lease_earlier_in_the_same_second_is_expired() {
    // Text: "…00Z" > "…00.5Z", yet the lease ended 500 ms before now.
    for op in OPS {
        assert!(
            expired(op, t0(), t0() + ms(500)).await,
            "{op:?}: an expired lease must not be skipped"
        );
    }
}

#[tokio::test]
async fn lease_ending_exactly_now_is_not_expired() {
    for op in OPS {
        assert!(
            !expired(op, t0() + ms(100), t0() + ms(100)).await,
            "{op:?}: the comparison is strict"
        );
    }
}

#[tokio::test]
async fn sub_millisecond_gaps_never_expire_early() {
    let now = t0() + ms(100) + Duration::nanoseconds(400_000);
    for op in OPS {
        // Lease ends 0.2 ms after now; SQLite rounds both to the same
        // millisecond, which must read as "not expired".
        assert!(
            !expired(op, now + Duration::nanoseconds(200_000), now).await,
            "{op:?}: lease 0.2 ms in the future"
        );
        // Lease ends 0.6 ms after now and straddles a millisecond boundary.
        assert!(
            !expired(op, now + Duration::nanoseconds(600_000), now).await,
            "{op:?}: lease 0.6 ms in the future"
        );
        // Clearly past (by a whole millisecond and more).
        assert!(
            expired(op, now - ms(2), now).await,
            "{op:?}: lease 2 ms in the past"
        );
    }
}

#[tokio::test]
async fn non_utc_now_is_compared_by_instant() {
    // `+15:00` is valid RFC3339 but unparseable by SQLite's date functions; the
    // compared `now` must still work, in both directions.
    let far_east = UtcOffset::from_hms(15, 0, 0).unwrap();
    for op in OPS {
        assert!(
            expired(op, t0(), (t0() + ms(500)).to_offset(far_east)).await,
            "{op:?}: expired lease, +15:00 now"
        );
        assert!(
            !expired(op, t0() + ms(500), t0().to_offset(far_east)).await,
            "{op:?}: valid lease, +15:00 now"
        );
    }
}

#[tokio::test]
async fn claim_writes_a_lease_the_reclaim_compares_by_instant() {
    // The claim's own lease write, with a sub-millisecond fraction and an
    // offset SQLite cannot parse, must round-trip through the reclaim: held
    // until its instant, released after it.
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let id = insert_feed_entry(&pool, source_id, "uid-claim", "hash-claim").await;
    let repo = FeedEntryRepo::new(pool.clone());
    let far_east = UtcOffset::from_hms(15, 0, 0).unwrap();
    let lease = t0() + ms(100) + Duration::nanoseconds(400_000);
    let request = ClaimRequest {
        owner: "worker-a".to_string(),
        now: t0().to_offset(far_east),
        lease_expires_at: lease.to_offset(far_east),
        batch_size: 1,
        max_attempts: MAX_ATTEMPTS,
    };
    assert_eq!(
        repo.claim_pending_fetch(&request, &[]).await.unwrap().len(),
        1
    );
    // Stored in UTC, rounded up to the millisecond (never shortened).
    assert_eq!(
        lease_text(&pool, id).await.as_deref(),
        Some("2026-11-20T10:20:00.101Z")
    );

    assert_eq!(repo.reclaim_expired_leases(lease).await.unwrap(), 0);
    assert_eq!(
        repo.reclaim_expired_leases(t0() + ms(101)).await.unwrap(),
        0
    );
    assert_eq!(
        state_and_owner(&pool, id).await,
        ("fetching".to_string(), Some("worker-a".to_string()))
    );
    assert_eq!(
        repo.reclaim_expired_leases(t0() + ms(102)).await.unwrap(),
        1
    );
    assert_eq!(
        state_and_owner(&pool, id).await,
        ("pending_fetch".to_string(), None)
    );
}
