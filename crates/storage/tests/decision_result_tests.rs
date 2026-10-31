//! decision_policies / decision_results on SQLite (ADR 0011 §3–§8): window
//! and identity scoping, claim / release / refund, maintenance, raw cleanup
//! and the table constraints.

mod common;

use std::{collections::HashSet, sync::Arc};

use rss_ai_news_storage::{
    DecisionArticle, DecisionAttemptRecord, DecisionExecution, DecisionFailure,
    DecisionFailureMode, DecisionIdentity, DecisionRawRecord, DecisionResultRepo,
    DecisionResultRepository, DecisionResultState, DecisionSuccess, NewRunEvent,
    RunEventRepository, StoragePool,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime, macros::datetime};

use common::{
    insert_article, insert_feed_entry, insert_rule, make_test_pool,
    make_test_pool_with_connections, seed_source,
};

const POLICY: &str = "0d302268de2a7a19cea1bcb51ae6774f0a0b3194dc359a9d92394f284c51667a";
const OTHER_POLICY: &str = "d282388a37a4f31d61bdea1c80df09d3c4a4080364af1ed6b917e6b48f88579e";

fn identity(policy: &str, execution: &str) -> DecisionIdentity {
    DecisionIdentity {
        category_key: "ai".to_string(),
        policy_version: policy.to_string(),
        execution_key: execution.to_string(),
    }
}

fn laya() -> DecisionExecution {
    DecisionExecution {
        provider: "laya".to_string(),
        requested_model: "multilingual".to_string(),
        laya_max_len: Some(2048),
    }
}

fn repo(pool: &SqlitePool) -> DecisionResultRepo {
    DecisionResultRepo::new_with_storage(StoragePool::Sqlite(pool.clone()))
}

/// `count` articles in category `ai`, created now.
async fn seed_articles(pool: &SqlitePool, count: usize) -> Vec<i64> {
    let rule_id = insert_rule(pool, "extractor", "extractor-decision", "sha-decision").await;
    let source_id = seed_source(pool).await;
    let mut ids = Vec::new();
    for index in 0..count {
        let entry = insert_feed_entry(
            pool,
            source_id,
            &format!("uid-decision-{index}"),
            &format!("hash-decision-{index}"),
        )
        .await;
        ids.push(
            insert_article(pool, &format!("content-decision-{index}"), entry, rule_id)
                .await
                .unwrap(),
        );
    }
    ids
}

async fn setup(count: usize) -> (SqlitePool, DecisionResultRepo, Vec<i64>) {
    let (_dir, pool) = make_test_pool().await;
    let ids = seed_articles(&pool, count).await;
    let repo = repo(&pool);
    let now = OffsetDateTime::now_utc();
    repo.upsert_policy(POLICY, "{}", now).await.unwrap();
    repo.upsert_policy(OTHER_POLICY, "{}", now).await.unwrap();
    (pool, repo, ids)
}

fn long_ago() -> OffsetDateTime {
    OffsetDateTime::now_utc() - Duration::days(365 * 30)
}

async fn state_of(pool: &SqlitePool, id: i64) -> (String, i64) {
    sqlx::query_as("SELECT state, attempt_count FROM decision_results WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn failure(mode: DecisionFailureMode) -> DecisionFailure {
    DecisionFailure {
        mode,
        kind: "decision_timeout".to_string(),
        message: "decision request timed out after 10s".to_string(),
        max_attempts: 2,
        attempt: DecisionAttemptRecord::default(),
    }
}

#[tokio::test]
async fn generation_is_scoped_by_window_category_state_and_identity() {
    let (pool, repo, ids) = setup(4).await;
    sqlx::query("UPDATE articles SET created_at = '2020-01-01 00:00:00' WHERE id = ?")
        .bind(ids[0])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE articles SET state = 'retired' WHERE id = ?")
        .bind(ids[1])
        .execute(&pool)
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let cutoff = now - Duration::hours(48);
    let first = identity(POLICY, "laya:multilingual:2048");
    assert_eq!(
        repo.generate_pending(&first, &laya(), cutoff, 10, now)
            .await
            .unwrap(),
        2
    );
    // The anti-join makes generation idempotent.
    assert_eq!(
        repo.generate_pending(&first, &laya(), cutoff, 10, now)
            .await
            .unwrap(),
        0
    );
    // Another identity has its own rows; another category has none.
    let second = identity(OTHER_POLICY, "laya:multilingual:2048");
    assert_eq!(
        repo.generate_pending(&second, &laya(), cutoff, 1, now)
            .await
            .unwrap(),
        1
    );
    let mut elsewhere = first.clone();
    elsewhere.category_key = "ml".to_string();
    assert_eq!(
        repo.generate_pending(&elsewhere, &laya(), long_ago(), 10, now)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn the_window_compares_whole_seconds_on_sqlite_text() {
    let (pool, repo, ids) = setup(3).await;
    // CURRENT_TIMESTAMP layout, RFC 3339 layout with sub-seconds, and a row
    // just before the cutoff second.
    for (id, created_at) in [
        (ids[0], "2026-10-31 12:00:00"),
        (ids[1], "2026-10-31T12:00:00.200Z"),
        (ids[2], "2026-10-31 11:59:59"),
    ] {
        sqlx::query("UPDATE articles SET created_at = ? WHERE id = ?")
            .bind(created_at)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let cutoff = datetime!(2026-10-31 12:00:00.800 UTC);
    let inserted = repo
        .generate_pending(
            &identity(POLICY, "laya:multilingual:2048"),
            &laya(),
            cutoff,
            10,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert_eq!(inserted, 2);
}

#[tokio::test]
async fn claim_takes_only_the_current_identity_and_counts_the_attempt() {
    let (pool, repo, _ids) = setup(3).await;
    let now = OffsetDateTime::now_utc();
    let current = identity(POLICY, "laya:multilingual:2048");
    let old = identity(OTHER_POLICY, "jev:jev-1.13.0");
    let jev = DecisionExecution {
        provider: "jev".to_string(),
        requested_model: "jev-1.13.0".to_string(),
        laya_max_len: None,
    };
    repo.generate_pending(&old, &jev, long_ago(), 10, now)
        .await
        .unwrap();
    repo.generate_pending(&current, &laya(), long_ago(), 2, now)
        .await
        .unwrap();
    let claimed = repo
        .claim(&current, "owner-a", now + Duration::minutes(2), 3, 10, now)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);
    assert!(claimed.iter().all(|row| row.policy_version == POLICY
        && row.execution_key == "laya:multilingual:2048"
        && row.attempt_count == 1));
    assert_eq!(state_of(&pool, claimed[0].id).await.0, "running");
    assert_eq!(
        repo.count_other_identity_pending(&current).await.unwrap(),
        3
    );
    assert!(
        repo.claim(&current, "owner-b", now + Duration::minutes(2), 3, 10, now)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_claims_are_disjoint() {
    let (_dir, pool) = make_test_pool_with_connections(4).await;
    seed_articles(&pool, 40).await;
    let repo = Arc::new(repo(&pool));
    let now = OffsetDateTime::now_utc();
    repo.upsert_policy(POLICY, "{}", now).await.unwrap();
    let current = identity(POLICY, "laya:multilingual:2048");
    repo.generate_pending(&current, &laya(), long_ago(), 40, now)
        .await
        .unwrap();
    let mut handles = Vec::new();
    for worker in 0..4 {
        let repo = Arc::clone(&repo);
        let current = current.clone();
        handles.push(tokio::spawn(async move {
            let now = OffsetDateTime::now_utc();
            repo.claim(
                &current,
                &format!("worker-{worker}"),
                now + Duration::minutes(2),
                3,
                15,
                now,
            )
            .await
            .unwrap()
        }));
    }
    let mut ids = Vec::new();
    for handle in handles {
        ids.extend(handle.await.unwrap().into_iter().map(|row| row.id));
    }
    let unique: HashSet<_> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len());
    assert_eq!(ids.len(), 40);
}

#[tokio::test]
async fn release_success_needs_the_owner_and_records_the_attempt() {
    let (pool, repo, _ids) = setup(1).await;
    let now = OffsetDateTime::now_utc();
    let current = identity(POLICY, "laya:multilingual:2048");
    repo.generate_pending(&current, &laya(), long_ago(), 1, now)
        .await
        .unwrap();
    let claimed = repo
        .claim(&current, "owner-a", now + Duration::minutes(2), 3, 1, now)
        .await
        .unwrap();
    let success = DecisionSuccess {
        answers_json: "{\"schema\":1,\"answers\":[]}".to_string(),
        reported_model: "laya-rl-agent".to_string(),
        routing_model: Some("multilingual".to_string()),
        attempt: DecisionAttemptRecord {
            endpoint_origin: Some("https://laya.example".to_string()),
            input_tokens: Some(322),
            body_truncated: Some(false),
            raw: Some(DecisionRawRecord {
                body: Some("{}".to_string()),
                expires_at: Some(now + Duration::days(30)),
                bytes: 2,
                sha256: Some("ab".repeat(32)),
                withheld: None,
            }),
            ..DecisionAttemptRecord::default()
        },
    };
    assert!(
        !repo
            .release_success(claimed[0].id, "owner-b", &success, now)
            .await
            .unwrap()
    );
    assert!(
        repo.release_success(claimed[0].id, "owner-a", &success, now)
            .await
            .unwrap()
    );
    let (state, owner, tokens, raw): (String, Option<String>, Option<i64>, Option<String>) =
        sqlx::query_as(
            "SELECT state, lease_owner, input_tokens, raw_response FROM decision_results WHERE id = ?",
        )
        .bind(claimed[0].id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (state.as_str(), owner, tokens, raw.as_deref()),
        ("succeeded", None, Some(322), Some("{}"))
    );
}

#[tokio::test]
async fn failures_charge_refund_or_terminate() {
    let (pool, repo, _ids) = setup(3).await;
    let now = OffsetDateTime::now_utc();
    let current = identity(POLICY, "laya:multilingual:2048");
    repo.generate_pending(&current, &laya(), long_ago(), 3, now)
        .await
        .unwrap();
    let lease = now + Duration::minutes(2);
    let claimed = repo
        .claim(&current, "owner", lease, 2, 3, now)
        .await
        .unwrap();
    let (charged, refunded, permanent) = (claimed[0].id, claimed[1].id, claimed[2].id);
    let release = |id, mode| {
        let repo = &repo;
        async move {
            repo.release_failure(id, "owner", &failure(mode), now)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        release(charged, DecisionFailureMode::Charge).await,
        Some(DecisionResultState::Pending)
    );
    assert_eq!(
        release(refunded, DecisionFailureMode::Refund).await,
        Some(DecisionResultState::Pending)
    );
    assert_eq!(
        release(permanent, DecisionFailureMode::Permanent).await,
        Some(DecisionResultState::Failed)
    );
    assert_eq!(state_of(&pool, charged).await, ("pending".to_string(), 1));
    assert_eq!(state_of(&pool, refunded).await, ("pending".to_string(), 0));
    // Second attempt of the charged row is its last (max_attempts = 2).
    let again = repo
        .claim(&current, "owner", lease, 2, 3, now)
        .await
        .unwrap();
    assert_eq!(again.len(), 2);
    assert_eq!(
        release(charged, DecisionFailureMode::Charge).await,
        Some(DecisionResultState::Failed)
    );
    assert_eq!(state_of(&pool, charged).await, ("failed".to_string(), 2));
    // Wrong owner: nothing happens.
    assert_eq!(
        repo.release_failure(
            refunded,
            "other",
            &failure(DecisionFailureMode::Refund),
            now
        )
        .await
        .unwrap(),
        None
    );
}

#[tokio::test]
async fn maintenance_reclaims_expired_leases_and_terminalizes_spent_rows() {
    let (pool, repo, _ids) = setup(2).await;
    let now = OffsetDateTime::now_utc();
    let current = identity(POLICY, "laya:multilingual:2048");
    repo.generate_pending(&current, &laya(), long_ago(), 2, now)
        .await
        .unwrap();
    let claimed = repo
        .claim(&current, "crashed", now - Duration::seconds(1), 1, 2, now)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);
    // Another identity's maintenance leaves them alone.
    let other = identity(OTHER_POLICY, "laya:multilingual:2048");
    assert_eq!(repo.reclaim_expired(&other, now).await.unwrap(), 0);
    assert_eq!(repo.reclaim_expired(&current, now).await.unwrap(), 2);
    assert_eq!(
        state_of(&pool, claimed[0].id).await,
        ("pending".to_string(), 1)
    );
    // attempt_count 1 >= max_attempts 1: spent.
    assert_eq!(
        repo.terminalize_exhausted(&current, 1, now).await.unwrap(),
        2
    );
    let (state, kind): (String, String) =
        sqlx::query_as("SELECT state, last_error_kind FROM decision_results WHERE id = ?")
            .bind(claimed[0].id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), kind.as_str()),
        ("failed", "retry_budget_exhausted")
    );
}

#[tokio::test]
async fn expired_raw_bodies_are_cleared_across_identities_once_and_in_bounded_batches() {
    let (pool, repo, _ids) = setup(3).await;
    let now = OffsetDateTime::now_utc();
    for (policy, execution) in [
        (POLICY, "laya:multilingual:2048"),
        (OTHER_POLICY, "laya:multilingual:2048"),
    ] {
        repo.generate_pending(&identity(policy, execution), &laya(), long_ago(), 3, now)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE decision_results SET raw_response = '{}', raw_response_expires_at = ?")
        .bind(now - Duration::days(1))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(repo.purge_expired_raw(now, 4).await.unwrap(), 4);
    assert_eq!(repo.purge_expired_raw(now, 4).await.unwrap(), 2);
    assert_eq!(repo.purge_expired_raw(now, 4).await.unwrap(), 0);
    let left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM decision_results WHERE raw_response IS NOT NULL OR raw_response_expires_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn constraints_reject_invalid_rows() {
    let (pool, _repo, ids) = setup(1).await;
    let insert = |provider: &'static str, max_len: Option<i64>, state: &'static str| {
        let pool = pool.clone();
        let article = ids[0];
        async move {
            sqlx::query(
                "INSERT INTO decision_results (article_id, category_key, policy_version, execution_key, provider, requested_model, laya_max_len, state) VALUES (?, 'ai', ?, 'x', ?, 'm', ?, ?)",
            )
            .bind(article)
            .bind(POLICY)
            .bind(provider)
            .bind(max_len)
            .bind(state)
            .execute(&pool)
            .await
        }
    };
    assert!(insert("laya", None, "pending").await.is_err());
    assert!(insert("laya", Some(100), "pending").await.is_err());
    assert!(insert("jev", Some(2048), "pending").await.is_err());
    assert!(insert("openai", None, "pending").await.is_err());
    assert!(
        insert("jev", None, "succeeded").await.is_err(),
        "no answers"
    );
    assert!(insert("jev", None, "filtered").await.is_err());
    assert!(insert("jev", None, "pending").await.is_ok());
    let null_policy = sqlx::query(
        "INSERT INTO decision_policies (policy_version, canonical_json) VALUES (NULL, '{}')",
    )
    .execute(&pool)
    .await;
    assert!(null_policy.is_err());
    let short_policy = sqlx::query(
        "INSERT INTO decision_policies (policy_version, canonical_json) VALUES ('abc', '{}')",
    )
    .execute(&pool)
    .await;
    assert!(short_policy.is_err());
}

#[tokio::test]
async fn the_repo_reads_article_text_and_writes_run_events() {
    let (pool, repo, ids) = setup(1).await;
    assert_eq!(
        repo.load_article(ids[0]).await.unwrap(),
        Some(DecisionArticle {
            title: "title".to_string(),
            body_text: "body".to_string(),
        })
    );
    assert_eq!(repo.load_article(ids[0] + 1000).await.unwrap(), None);
    let id = repo
        .insert(&NewRunEvent {
            run_id: "run-1".to_string(),
            trace_id: None,
            stage: "decision_run".to_string(),
            severity: "info".to_string(),
            event_kind: "run_started".to_string(),
            target_kind: None,
            target_id: None,
            message: "started".to_string(),
            context_json: Some("{}".to_string()),
        })
        .await
        .unwrap();
    let stage: String = sqlx::query_scalar("SELECT stage FROM run_events WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stage, "decision_run");
}
