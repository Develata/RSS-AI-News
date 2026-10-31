//! decision_results on PostgreSQL (ADR 0011): window on TIMESTAMPTZ, claim
//! with `FOR UPDATE SKIP LOCKED`, the three release modes, maintenance, raw
//! cleanup and constraints. Default `#[ignore]`; needs docker (the postgres
//! acceptance lane runs it with `--include-ignored`).

mod common;

use std::collections::HashSet;

use common::pg::{PgTestContext, make_pg_test_pool};
use rss_ai_news_storage::{
    DecisionAttemptRecord, DecisionExecution, DecisionFailure, DecisionFailureMode,
    DecisionIdentity, DecisionResultRepo, DecisionResultRepository, DecisionResultState,
    DecisionSuccess,
};
use time::{Duration, OffsetDateTime};

const POLICY: &str = "0d302268de2a7a19cea1bcb51ae6774f0a0b3194dc359a9d92394f284c51667a";

fn identity() -> DecisionIdentity {
    DecisionIdentity {
        category_key: "ai".to_string(),
        policy_version: POLICY.to_string(),
        execution_key: "laya:multilingual:2048".to_string(),
    }
}

fn laya() -> DecisionExecution {
    DecisionExecution {
        provider: "laya".to_string(),
        requested_model: "multilingual".to_string(),
        laya_max_len: Some(2048),
    }
}

async fn seed_articles(ctx: &PgTestContext, count: usize) -> Vec<i64> {
    let config_rule: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('config', 'cfg-decision', 'c', 'sha-cfg-decision', 'superseded') RETURNING id",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
            feed_kind, config_version) \
         VALUES ('ai', 'src-decision', 'AI Main', 'https://example.com/feed.xml', 'rss', $1) \
         RETURNING id",
    )
    .bind(config_rule)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let ext_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', 'ext-decision', 'e', 'sha-ext-decision', 'superseded') RETURNING id",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let mut ids = Vec::new();
    for index in 0..count {
        let entry_id: i64 = sqlx::query_scalar(
            "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
                title_raw, discovered_at, state, dedup_decision) \
             VALUES ($1, $2, $3, $4, 'title', $5, 'pending_fetch', 'fresh') RETURNING id",
        )
        .bind(source_id)
        .bind(format!("uid-decision-{index}"))
        .bind(format!("https://example.com/decision-{index}"))
        .bind(format!("link-hash-decision-{index}"))
        .bind(OffsetDateTime::now_utc())
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
        ids.push(
            sqlx::query_scalar(
                "INSERT INTO articles (content_hash, canonical_link, title, body_text, \
                    extractor_strategy, extractor_version, content_quality, origin_feed_entry_id) \
                 VALUES ($1, 'https://example.com/a', 'title', 'body', 'readability', $2, 'high', $3) \
                 RETURNING id",
            )
            .bind(format!("hash-decision-{index}"))
            .bind(ext_id)
            .bind(entry_id)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap(),
        );
    }
    ids
}

async fn setup(count: usize) -> (PgTestContext, DecisionResultRepo, Vec<i64>) {
    let ctx = make_pg_test_pool().await;
    let ids = seed_articles(&ctx, count).await;
    let repo = DecisionResultRepo::new_with_storage(ctx.storage_pool().clone());
    repo.upsert_policy(POLICY, "{}", OffsetDateTime::now_utc())
        .await
        .unwrap();
    (ctx, repo, ids)
}

#[tokio::test]
#[ignore = "needs docker"]
async fn pg_window_uses_whole_second_cutoff_and_generation_is_idempotent() {
    let (ctx, repo, ids) = setup(3).await;
    for (id, created_at) in [
        (ids[0], "2026-10-31 12:00:00+00"),
        (ids[1], "2026-10-31 12:00:00.200+00"),
        (ids[2], "2026-10-31 11:59:59.900+00"),
    ] {
        sqlx::query("UPDATE articles SET created_at = $1::timestamptz WHERE id = $2")
            .bind(created_at)
            .bind(id)
            .execute(ctx.pg_pool())
            .await
            .unwrap();
    }
    let cutoff = time::macros::datetime!(2026-10-31 12:00:00.800 UTC);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        repo.generate_pending(&identity(), &laya(), cutoff, 10, now)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        repo.generate_pending(&identity(), &laya(), cutoff, 10, now)
            .await
            .unwrap(),
        0
    );
    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "needs docker"]
async fn pg_parallel_claims_are_disjoint_and_releases_follow_their_mode() {
    let (ctx, repo, _ids) = setup(12).await;
    let now = OffsetDateTime::now_utc();
    let long_ago = now - Duration::days(3650);
    repo.generate_pending(&identity(), &laya(), long_ago, 12, now)
        .await
        .unwrap();
    let lease = now + Duration::minutes(2);
    let current = identity();
    let (first, second) = tokio::join!(
        repo.claim(&current, "a", lease, 2, 6, now),
        repo.claim(&current, "b", lease, 2, 6, now),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    let ids: HashSet<_> = first.iter().chain(&second).map(|row| row.id).collect();
    assert_eq!(ids.len(), first.len() + second.len());
    assert_eq!(ids.len(), 12);
    // SKIP LOCKED may split the rows unevenly; continue with the larger share.
    let (first, owner, other) = if first.len() >= 4 {
        (first, "a", "b")
    } else {
        (second, "b", "a")
    };

    let failure = |mode| DecisionFailure {
        mode,
        kind: "decision_timeout".to_string(),
        message: "timed out".to_string(),
        max_attempts: 2,
        attempt: DecisionAttemptRecord::default(),
    };
    let release = |id, mode| {
        let failure = failure(mode);
        let repo = &repo;
        async move { repo.release_failure(id, owner, &failure, now).await }
    };
    assert_eq!(
        release(first[0].id, DecisionFailureMode::Charge)
            .await
            .unwrap(),
        Some(DecisionResultState::Pending)
    );
    assert_eq!(
        release(first[1].id, DecisionFailureMode::Refund)
            .await
            .unwrap(),
        Some(DecisionResultState::Pending)
    );
    assert_eq!(
        release(first[2].id, DecisionFailureMode::Permanent)
            .await
            .unwrap(),
        Some(DecisionResultState::Failed)
    );
    let refunded: i64 =
        sqlx::query_scalar("SELECT attempt_count FROM decision_results WHERE id = $1")
            .bind(first[1].id)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
    assert_eq!(refunded, 0);
    assert_eq!(
        repo.release_failure(
            first[3].id,
            other,
            &failure(DecisionFailureMode::Charge),
            now
        )
        .await
        .unwrap(),
        None
    );
    let success = DecisionSuccess {
        answers_json: "{}".to_string(),
        reported_model: "laya-rl-agent".to_string(),
        routing_model: Some("multilingual".to_string()),
        attempt: DecisionAttemptRecord {
            body_truncated: Some(true),
            provider_truncated: Some(false),
            ..DecisionAttemptRecord::default()
        },
    };
    assert!(
        repo.release_success(first[3].id, owner, &success, now)
            .await
            .unwrap()
    );
    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "needs docker"]
async fn pg_maintenance_and_raw_cleanup() {
    let (ctx, repo, _ids) = setup(3).await;
    let now = OffsetDateTime::now_utc();
    repo.generate_pending(&identity(), &laya(), now - Duration::days(3650), 3, now)
        .await
        .unwrap();
    let claimed = repo
        .claim(
            &identity(),
            "crashed",
            now - Duration::seconds(1),
            1,
            3,
            now,
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 3);
    assert_eq!(repo.reclaim_expired(&identity(), now).await.unwrap(), 3);
    assert_eq!(
        repo.terminalize_exhausted(&identity(), 1, now)
            .await
            .unwrap(),
        3
    );
    sqlx::query("UPDATE decision_results SET raw_response = '{}', raw_response_expires_at = $1")
        .bind(now - Duration::days(1))
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(repo.purge_expired_raw(now, 2).await.unwrap(), 2);
    assert_eq!(repo.purge_expired_raw(now, 2).await.unwrap(), 1);
    assert_eq!(repo.purge_expired_raw(now, 2).await.unwrap(), 0);
    let laya_without_len = sqlx::query(
        "INSERT INTO decision_results (article_id, category_key, policy_version, execution_key, \
            provider, requested_model, laya_max_len) VALUES ($1, 'ai', $2, 'y', 'laya', 'm', NULL)",
    )
    .bind(claimed[0].article_id)
    .bind(POLICY)
    .execute(ctx.pg_pool())
    .await;
    assert!(laya_without_len.is_err());
    let article = repo
        .load_article(claimed[0].article_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(article.body_text, "body");
    let event_id = rss_ai_news_storage::RunEventRepository::insert(
        &repo,
        &rss_ai_news_storage::NewRunEvent {
            run_id: "run-1".to_string(),
            trace_id: None,
            stage: "decision_run".to_string(),
            severity: "info".to_string(),
            event_kind: "run_started".to_string(),
            target_kind: None,
            target_id: None,
            message: "started".to_string(),
            context_json: None,
        },
    )
    .await
    .unwrap();
    assert!(event_id > 0);
    ctx.cleanup().await;
}
