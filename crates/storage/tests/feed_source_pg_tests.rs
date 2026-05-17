//! W11-P3-C-1：[`FeedSourceRepo`] PG 分支冒烟。
//!
//! 用 [`common::pg::make_pg_test_pool`] 拉 per-test schema，验证 PG 路径
//! 与 SQLite 行为等价（不并排跑 rstest 双轨；P3-C-4 末尾做 4 repo × 1-2
//! 用例的最小双轨集，避免本阶段测试规模膨胀）。
//!
//! 覆盖：
//!   - `upsert` → `find_by_id` roundtrip（PG ON CONFLICT + RETURNING）
//!   - `list_by_category` 仅返回 active
//!   - `mark_archived` 软删 + 二次返 false
//!   - `upsert_with_lease_guard` 无 lease → LeaseLost；持 lease → Applied + 真写
//!   - `mark_archived_with_lease_guard` 持 lease → Applied；二次 → NoOp
//!
//! 默认 `#[ignore]`，需要 docker。

mod common;

use common::pg::{PgTestContext, make_pg_test_pool};
use rss_ai_news_domain::{
    model::FeedSource,
    state::{FeedKind, FeedSourceStatus},
};
use rss_ai_news_storage::{FeedSourceRepo, FeedSourceRepository, LeaseGuardedWriteOutcome};
use time::OffsetDateTime;

fn sample_feed_source(category: &str, source: &str, config_version: i64) -> FeedSource {
    let now = OffsetDateTime::now_utc();
    FeedSource {
        id: 0,
        category_key: category.to_string(),
        source_key: source.to_string(),
        display_name: "sample".to_string(),
        feed_url: "https://example.com/feed.xml".to_string(),
        feed_kind: FeedKind::Rss,
        status: FeedSourceStatus::Active,
        priority: 10,
        etag: None,
        last_modified: None,
        last_fetched_at: None,
        last_success_at: None,
        consecutive_failures: 0,
        last_error: None,
        last_error_kind: None,
        config_version,
        created_at: now,
        updated_at: now,
    }
}

/// 在 PG fixture 上 seed 一条 `rule_versions(status='superseded')`，
/// 返回 rule_version_id。`config_version` 外键所需。
async fn seed_rule_version(ctx: &PgTestContext, tag: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('config', $1, 'pg test rule', $2, 'superseded') RETURNING id",
    )
    .bind(tag)
    .bind(format!("sha-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .expect("seed rule_versions")
}

/// 在 PG fixture 上 seed 一条 `reindex_jobs(state='running', lease_owner=:owner)`。
/// 用于 lease-guard 路径正面用例。返回 job_id。
async fn seed_running_reindex_job(ctx: &PgTestContext, rule_version_id: i64, owner: &str) -> i64 {
    let now = OffsetDateTime::now_utc();
    let lease_expires = now + time::Duration::minutes(5);
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO reindex_jobs (target, rule_version_id, state, lease_owner, \
            lease_expires_at, attempt_count, started_at, created_at, updated_at) \
         VALUES ('categories', $1, 'running', $2, $3, 1, $4, $4, $4) RETURNING id",
    )
    .bind(rule_version_id)
    .bind(owner)
    .bind(lease_expires)
    .bind(now)
    .fetch_one(ctx.pg_pool())
    .await
    .expect("seed reindex_jobs running")
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_upsert_then_find_by_id_roundtrip() {
    let ctx = make_pg_test_pool().await;
    let rule_id = seed_rule_version(&ctx, "v1").await;
    let repo = FeedSourceRepo::new_with_storage(ctx.storage_pool().clone());

    let src = sample_feed_source("ai", "main", rule_id);
    let id = repo.upsert(&src).await.expect("pg upsert");
    assert!(id > 0, "upsert returns positive id");

    let found = repo
        .find_by_id(id)
        .await
        .expect("pg find_by_id")
        .expect("inserted row present");
    assert_eq!(found.category_key, "ai");
    assert_eq!(found.source_key, "main");
    assert_eq!(found.feed_kind, FeedKind::Rss);
    assert_eq!(found.status, FeedSourceStatus::Active);
    assert_eq!(found.config_version, rule_id);

    // 二次 upsert 走 ON CONFLICT DO UPDATE：返同一 id，display_name 已更新
    let mut updated = src.clone();
    updated.display_name = "renamed".to_string();
    updated.updated_at = OffsetDateTime::now_utc();
    let id2 = repo.upsert(&updated).await.expect("pg upsert update");
    assert_eq!(id2, id, "ON CONFLICT DO UPDATE returns existing id");
    let after = repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(after.display_name, "renamed");
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_list_by_category_filters_to_active() {
    let ctx = make_pg_test_pool().await;
    let rule_id = seed_rule_version(&ctx, "v1").await;
    let repo = FeedSourceRepo::new_with_storage(ctx.storage_pool().clone());

    let active = sample_feed_source("ai", "active-source", rule_id);
    let mut archived = sample_feed_source("ai", "archived-source", rule_id);
    archived.status = FeedSourceStatus::Archived;
    repo.upsert(&active).await.unwrap();
    repo.upsert(&archived).await.unwrap();

    let listed = repo
        .list_by_category("ai")
        .await
        .expect("pg list_by_category");
    assert_eq!(listed.len(), 1, "only active source listed; got {listed:?}");
    assert_eq!(listed[0].source_key, "active-source");
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_mark_archived_then_second_call_returns_false() {
    let ctx = make_pg_test_pool().await;
    let rule_id = seed_rule_version(&ctx, "v1").await;
    let repo = FeedSourceRepo::new_with_storage(ctx.storage_pool().clone());

    let src = sample_feed_source("ai", "to-archive", rule_id);
    let id = repo.upsert(&src).await.unwrap();
    assert!(
        repo.mark_archived(id).await.unwrap(),
        "first archive applied"
    );
    assert!(
        !repo.mark_archived(id).await.unwrap(),
        "second archive is no-op (WHERE status <> 'archived')"
    );

    let after = repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(after.status, FeedSourceStatus::Archived);
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_upsert_with_lease_guard_without_lease_returns_lease_lost() {
    let ctx = make_pg_test_pool().await;
    let rule_id = seed_rule_version(&ctx, "v1").await;
    let repo = FeedSourceRepo::new_with_storage(ctx.storage_pool().clone());

    let src = sample_feed_source("ai", "no-lease", rule_id);
    let outcome = repo
        .upsert_with_lease_guard(&src, 9999, "owner-x", OffsetDateTime::now_utc())
        .await
        .expect("pg upsert_with_lease_guard");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::LeaseLost);

    // 事务回滚：feed_sources 这一行不应该存在
    let found = repo
        .find_by_keys("ai", "no-lease")
        .await
        .expect("pg find_by_keys");
    assert!(found.is_none(), "lease lost rolls back feed_sources INSERT");
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_upsert_with_lease_guard_applied_writes_row_in_same_tx() {
    let ctx = make_pg_test_pool().await;
    let rule_id = seed_rule_version(&ctx, "v1").await;
    let owner = "owner-pg-1";
    let job_id = seed_running_reindex_job(&ctx, rule_id, owner).await;
    let repo = FeedSourceRepo::new_with_storage(ctx.storage_pool().clone());

    let src = sample_feed_source("ai", "lease-ok", rule_id);
    let outcome = repo
        .upsert_with_lease_guard(&src, job_id, owner, OffsetDateTime::now_utc())
        .await
        .expect("pg upsert_with_lease_guard");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::Applied);

    let found = repo
        .find_by_keys("ai", "lease-ok")
        .await
        .unwrap()
        .expect("lease-held upsert writes feed_sources row");
    assert_eq!(found.display_name, "sample");
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_mark_archived_with_lease_guard_second_call_is_noop() {
    let ctx = make_pg_test_pool().await;
    let rule_id = seed_rule_version(&ctx, "v1").await;
    let owner = "owner-pg-2";
    let job_id = seed_running_reindex_job(&ctx, rule_id, owner).await;
    let repo = FeedSourceRepo::new_with_storage(ctx.storage_pool().clone());

    let src = sample_feed_source("ai", "target", rule_id);
    let id = repo.upsert(&src).await.unwrap();

    let now = OffsetDateTime::now_utc();
    let first = repo
        .mark_archived_with_lease_guard(id, job_id, owner, now)
        .await
        .expect("first lease-guarded archive");
    assert_eq!(first, LeaseGuardedWriteOutcome::Applied);

    let second = repo
        .mark_archived_with_lease_guard(id, job_id, owner, now)
        .await
        .expect("second lease-guarded archive (target already archived)");
    assert_eq!(
        second,
        LeaseGuardedWriteOutcome::NoOp,
        "second archive is no-op (status <> 'archived' filters out)"
    );
}
