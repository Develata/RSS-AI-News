//! C5 — lease / TTL comparisons on `article_ai_results` and
//! `decision_results` (SQLite) are by instant, not by text.
//!
//! Same premise as `feed_entry_lease_instant_tests.rs`: sqlx stores
//! `OffsetDateTime` as RFC3339 TEXT with a variable-width fraction, so within
//! one second text order differs from instant order (`…00.5Z` < `…00Z`,
//! `…00.51Z` < `…00.5Z`). Every `… < now` predicate (claim, reclaim, budget
//! sweep, raw-response purge) must follow the instants, stay strict, and
//! never expire early.

mod common;

use rss_ai_news_storage::{
    ArticleAiResultRepo, ArticleAiResultRepository, ClaimRequest, DecisionIdentity,
    DecisionResultRepo, DecisionResultRepository, StoragePool,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_rule, seed_article};

const MAX_ATTEMPTS: u32 = 3;
const POLICY: &str = "0d302268de2a7a19cea1bcb51ae6774f0a0b3194dc359a9d92394f284c51667a";

/// 2026-11-20T10:20:00Z, a whole second.
fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1795170000).expect("valid timestamp")
}

fn ms(n: i64) -> Duration {
    Duration::milliseconds(n)
}

#[derive(Clone, Copy, Debug)]
enum Op {
    AiClaim,
    /// C9 fresh-share claim: same lease predicate.
    AiClaimFresh,
    AiReclaim,
    AiTerminalize,
    DecisionClaim,
    DecisionReclaim,
    DecisionTerminalize,
    DecisionPurgeRaw,
}

const OPS: [Op; 8] = [
    Op::AiClaim,
    Op::AiClaimFresh,
    Op::AiReclaim,
    Op::AiTerminalize,
    Op::DecisionClaim,
    Op::DecisionReclaim,
    Op::DecisionTerminalize,
    Op::DecisionPurgeRaw,
];

fn identity() -> DecisionIdentity {
    DecisionIdentity {
        category_key: "ai".to_string(),
        policy_version: POLICY.to_string(),
        execution_key: "laya:multilingual:2048".to_string(),
    }
}

/// One AI row in `state` with `attempts`, leased until `deadline`.
async fn seed_ai(pool: &SqlitePool, state: &str, attempts: u32, deadline: OffsetDateTime) {
    let (article_id, _, _) = seed_article(pool).await;
    let prompt = insert_rule(pool, "prompt", "p-instant", "sha-p-instant").await;
    let schema = insert_rule(pool, "ai_output_schema", "s-instant", "sha-s-instant").await;
    sqlx::query(
        "INSERT INTO article_ai_results (article_id, prompt_version, output_schema_version, \
         model_id, state, attempt_count, lease_owner, lease_expires_at) \
         VALUES (?, ?, ?, 'm', ?, ?, 'worker-old', ?)",
    )
    .bind(article_id)
    .bind(prompt)
    .bind(schema)
    .bind(state)
    .bind(i64::from(attempts))
    .bind(deadline)
    .execute(pool)
    .await
    .expect("AI row should insert");
}

/// One decision row; `deadline` is its lease, or its raw-response expiry for
/// the purge.
async fn seed_decision(pool: &SqlitePool, op: Op, deadline: OffsetDateTime) {
    let (article_id, _, _) = seed_article(pool).await;
    let repo = DecisionResultRepo::new_with_storage(StoragePool::Sqlite(pool.clone()));
    repo.upsert_policy(POLICY, "{}", t0()).await.unwrap();
    let (state, attempts, lease, raw, raw_expires) = match op {
        Op::DecisionClaim => ("pending", 0, Some(deadline), None, None),
        Op::DecisionReclaim => ("running", 1, Some(deadline), None, None),
        Op::DecisionTerminalize => ("pending", MAX_ATTEMPTS, Some(deadline), None, None),
        Op::DecisionPurgeRaw => ("failed", 1, None, Some("{}"), Some(deadline)),
        _ => unreachable!(),
    };
    sqlx::query(
        "INSERT INTO decision_results (article_id, category_key, policy_version, execution_key, \
         provider, requested_model, laya_max_len, state, attempt_count, lease_owner, \
         lease_expires_at, raw_response, raw_response_expires_at) \
         VALUES (?, 'ai', ?, 'laya:multilingual:2048', 'laya', 'multilingual', 2048, ?, ?, \
                 'worker-old', ?, ?, ?)",
    )
    .bind(article_id)
    .bind(POLICY)
    .bind(state)
    .bind(i64::from(attempts))
    .bind(lease)
    .bind(raw)
    .bind(raw_expires)
    .execute(pool)
    .await
    .expect("decision row should insert");
}

/// Seeds one row for `op`, runs `op` at `now`, and reports whether the
/// deadline was treated as passed.
async fn expired(op: Op, deadline: OffsetDateTime, now: OffsetDateTime) -> bool {
    let (_dir, pool) = common::make_test_pool().await;
    let ai = ArticleAiResultRepo::new(pool.clone());
    let decision = DecisionResultRepo::new_with_storage(StoragePool::Sqlite(pool.clone()));
    match op {
        Op::AiClaim | Op::AiClaimFresh => seed_ai(&pool, "pending", 0, deadline).await,
        Op::AiReclaim => seed_ai(&pool, "running", 1, deadline).await,
        Op::AiTerminalize => seed_ai(&pool, "pending", MAX_ATTEMPTS, deadline).await,
        _ => seed_decision(&pool, op, deadline).await,
    }
    match op {
        Op::AiClaim => {
            let request = ClaimRequest {
                owner: "worker-new".to_string(),
                now,
                lease_expires_at: now + Duration::minutes(5),
                batch_size: 10,
                max_attempts: MAX_ATTEMPTS,
            };
            ai.claim_pending(&request, "ai").await.unwrap().len() == 1
        }
        Op::AiClaimFresh => {
            let request = ClaimRequest {
                owner: "worker-new".to_string(),
                now,
                lease_expires_at: now + Duration::minutes(5),
                batch_size: 10,
                max_attempts: MAX_ATTEMPTS,
            };
            // The seeded article was discovered just now: inside any window.
            ai.claim_pending_fresh(&request, "ai", t0() - Duration::hours(48))
                .await
                .unwrap()
                .len()
                == 1
        }
        Op::AiReclaim => ai.reclaim_expired_leases(now).await.unwrap() == 1,
        Op::AiTerminalize => ai.terminalize_exhausted(MAX_ATTEMPTS, now).await.unwrap() == 1,
        Op::DecisionClaim => {
            let claimed = decision
                .claim(
                    &identity(),
                    "worker-new",
                    now + Duration::minutes(5),
                    MAX_ATTEMPTS,
                    10,
                    now,
                )
                .await
                .unwrap();
            claimed.len() == 1
        }
        Op::DecisionReclaim => decision.reclaim_expired(&identity(), now).await.unwrap() == 1,
        Op::DecisionTerminalize => {
            decision
                .terminalize_exhausted(&identity(), MAX_ATTEMPTS, now)
                .await
                .unwrap()
                == 1
        }
        Op::DecisionPurgeRaw => decision.purge_expired_raw(now, 10).await.unwrap() == 1,
    }
}

#[tokio::test]
async fn half_second_deadline_is_not_passed_at_the_whole_second() {
    // Text: "…00.5Z" < "…00Z", yet the deadline is 500 ms after now.
    for op in OPS {
        assert!(
            !expired(op, t0() + ms(500), t0()).await,
            "{op:?}: a deadline in the future must not count as passed"
        );
    }
}

#[tokio::test]
async fn longer_fraction_later_in_the_second_is_not_passed() {
    // Text: "…00.51Z" < "…00.5Z", yet the deadline is 10 ms after now.
    for op in OPS {
        assert!(
            !expired(op, t0() + ms(510), t0() + ms(500)).await,
            "{op:?}: .51 is later than .5"
        );
    }
}

#[tokio::test]
async fn whole_second_deadline_has_passed_half_a_second_later() {
    // Text: "…00Z" > "…00.5Z", yet the deadline ended 500 ms before now.
    for op in OPS {
        assert!(
            expired(op, t0(), t0() + ms(500)).await,
            "{op:?}: a passed deadline must not be skipped"
        );
    }
}

#[tokio::test]
async fn deadline_exactly_now_has_not_passed() {
    for op in OPS {
        assert!(
            !expired(op, t0() + ms(500), t0() + ms(500)).await,
            "{op:?}: the comparison is strict"
        );
    }
}

#[tokio::test]
async fn sub_millisecond_deadlines_never_pass_early() {
    let now = t0() + ms(100) + Duration::nanoseconds(400_000);
    for op in OPS {
        assert!(
            !expired(op, now + Duration::nanoseconds(600_000), now).await,
            "{op:?}: deadline 0.6 ms in the future"
        );
        assert!(
            expired(op, now - ms(2), now).await,
            "{op:?}: deadline 2 ms in the past"
        );
    }
}

#[tokio::test]
async fn claims_store_leases_in_utc_rounded_up_to_the_millisecond() {
    let (_dir, pool) = common::make_test_pool().await;
    seed_ai(&pool, "pending", 0, t0() - ms(1)).await;
    let lease = t0() + ms(100) + Duration::nanoseconds(400_000);
    let request = ClaimRequest {
        owner: "worker-a".to_string(),
        now: t0(),
        lease_expires_at: lease.to_offset(time::UtcOffset::from_hms(15, 0, 0).unwrap()),
        batch_size: 1,
        max_attempts: MAX_ATTEMPTS,
    };
    let ai = ArticleAiResultRepo::new(pool.clone());
    assert_eq!(ai.claim_pending(&request, "ai").await.unwrap().len(), 1);
    let stored: String =
        sqlx::query_scalar("SELECT CAST(lease_expires_at AS TEXT) FROM article_ai_results")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "2026-11-20T10:20:00.101Z");
    // Held through its instant, released after it.
    assert_eq!(ai.reclaim_expired_leases(t0() + ms(101)).await.unwrap(), 0);
    assert_eq!(ai.reclaim_expired_leases(t0() + ms(102)).await.unwrap(), 1);

    let (_dir2, pool) = common::make_test_pool().await;
    seed_decision(&pool, Op::DecisionClaim, t0() - ms(1)).await;
    let decision = DecisionResultRepo::new_with_storage(StoragePool::Sqlite(pool.clone()));
    let claimed = decision
        .claim(&identity(), "worker-a", lease, MAX_ATTEMPTS, 1, t0())
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let stored: String =
        sqlx::query_scalar("SELECT CAST(lease_expires_at AS TEXT) FROM decision_results")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "2026-11-20T10:20:00.101Z");
}
