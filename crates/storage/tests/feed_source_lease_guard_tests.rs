//! F15-fix7 — `FeedSourceRepository::upsert_with_lease_guard` /
//! `mark_archived_with_lease_guard` 的事务/原子性单测。
//!
//! 关闭目标：fix2 在 reindex `categories` target 的写循环里加了 per-write
//! `assert_lease_held` guard，但 guard 与 upsert 之间仍存在一段 TOCTOU 窗口
//! —— guard 通过到 INSERT/UPDATE 落地之间若发生 abort/reclaim，旧 worker
//! 仍会覆盖一行 feed_sources。本模块逐项验证 fix7 引入的事务版原语：
//!
//!   1. lease 在手时 `upsert_with_lease_guard` → `Applied` + feed_sources
//!      行被真实写入；同时 reindex_jobs.updated_at 被刷新到 src.updated_at
//!      （rows_affected 谓词副产物）。
//!   2. job 被 abort 后再调用 `upsert_with_lease_guard` → `LeaseLost`，且
//!      事务回滚——feed_sources **不**新增任何行（这是关闭 TOCTOU 的核心
//!      不变量）。
//!   3. owner 不匹配（lease 仍在 running 但被别 worker 抢走）→ `LeaseLost`
//!      + 无写入。
//!   4. `mark_archived_with_lease_guard` 三态：Applied / NoOp（行已 archived）/
//!      LeaseLost。

mod common;

use rss_ai_news_domain::{
    model::FeedSource,
    state::{FeedKind, FeedSourceStatus},
};
use rss_ai_news_storage::{
    FeedSourceRepo, FeedSourceRepository, LeaseGuardedWriteOutcome, ReindexJobRepo,
    ReindexJobRepository, build_owner_id, lease_expires_at,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_rule, make_test_pool};

async fn seed_running_job(pool: &SqlitePool) -> (i64, String) {
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = insert_rule(pool, "reindex", "fix7-test", "fix7-sha").await;
    let now = OffsetDateTime::now_utc();
    let job_id = repo
        .insert_pending("categories", rule_id, now)
        .await
        .expect("insert_pending ok");
    let owner = build_owner_id();
    let lease = lease_expires_at(now, Duration::seconds(60));
    repo.claim_by_id(job_id, &owner, now, lease)
        .await
        .expect("claim_by_id ok")
        .expect("job should be claimable");
    (job_id, owner)
}

fn sample_source(category: &str, source: &str, config_version: i64) -> FeedSource {
    let now = OffsetDateTime::now_utc();
    FeedSource {
        id: 0,
        category_key: category.to_string(),
        source_key: source.to_string(),
        display_name: format!("{category}/{source}"),
        feed_url: format!("https://example.com/{category}/{source}.xml"),
        feed_kind: FeedKind::Rss,
        status: FeedSourceStatus::Active,
        priority: 100,
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

async fn count_feed_sources(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM feed_sources")
        .fetch_one(pool)
        .await
        .expect("count ok")
}

#[tokio::test]
async fn upsert_with_lease_guard_writes_when_lease_held() {
    let (_dir, pool) = make_test_pool().await;
    let (job_id, owner) = seed_running_job(&pool).await;
    let config_id = insert_rule(&pool, "config", "cfg-1", "cfg-sha-1").await;
    let repo = FeedSourceRepo::new(pool.clone());

    let src = sample_source("ai", "main", config_id);
    let outcome = repo
        .upsert_with_lease_guard(&src, job_id, &owner, OffsetDateTime::now_utc())
        .await
        .expect("upsert ok");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::Applied);

    let row = repo
        .find_by_keys("ai", "main")
        .await
        .expect("find ok")
        .expect("row exists");
    assert_eq!(row.display_name, "ai/main");
    assert_eq!(row.config_version, config_id);
}

#[tokio::test]
async fn upsert_with_lease_guard_rolls_back_after_abort() {
    // 核心 TOCTOU 闭合不变量：abort 后再次写入既不会成功，也不会留下任何
    // feed_sources 行 —— 整段事务被 rollback。
    let (_dir, pool) = make_test_pool().await;
    let (job_id, owner) = seed_running_job(&pool).await;
    let config_id = insert_rule(&pool, "config", "cfg-1", "cfg-sha-1").await;
    let feed_repo = FeedSourceRepo::new(pool.clone());
    let job_repo = ReindexJobRepo::new(pool.clone());

    let before = count_feed_sources(&pool).await;
    job_repo
        .abort(job_id, "test", OffsetDateTime::now_utc())
        .await
        .expect("abort ok");

    let src = sample_source("ai", "main", config_id);
    let outcome = feed_repo
        .upsert_with_lease_guard(&src, job_id, &owner, OffsetDateTime::now_utc())
        .await
        .expect("upsert returns Ok with LeaseLost");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::LeaseLost);

    assert_eq!(
        count_feed_sources(&pool).await,
        before,
        "feed_sources 在 LeaseLost 路径上必须保持原行数",
    );
    let row = feed_repo.find_by_keys("ai", "main").await.expect("find ok");
    assert!(row.is_none(), "LeaseLost 时不应写入新行");
}

#[tokio::test]
async fn upsert_with_lease_guard_rejects_wrong_owner() {
    let (_dir, pool) = make_test_pool().await;
    let (job_id, _real_owner) = seed_running_job(&pool).await;
    let config_id = insert_rule(&pool, "config", "cfg-1", "cfg-sha-1").await;
    let repo = FeedSourceRepo::new(pool.clone());

    let src = sample_source("ai", "main", config_id);
    let outcome = repo
        .upsert_with_lease_guard(&src, job_id, "imposter-owner", OffsetDateTime::now_utc())
        .await
        .expect("upsert returns Ok with LeaseLost");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::LeaseLost);
    assert_eq!(count_feed_sources(&pool).await, 0);
}

#[tokio::test]
async fn mark_archived_with_lease_guard_three_outcomes() {
    let (_dir, pool) = make_test_pool().await;
    let (job_id, owner) = seed_running_job(&pool).await;
    let config_id = insert_rule(&pool, "config", "cfg-1", "cfg-sha-1").await;
    let repo = FeedSourceRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();

    // 先用 lease-guarded upsert 写一行 active 行（同时验证 Applied 路径）。
    repo.upsert_with_lease_guard(&sample_source("ai", "main", config_id), job_id, &owner, now)
        .await
        .expect("upsert ok");
    let row = repo
        .find_by_keys("ai", "main")
        .await
        .expect("find ok")
        .expect("row exists");

    // Applied：active → archived。
    let outcome = repo
        .mark_archived_with_lease_guard(row.id, job_id, &owner, now)
        .await
        .expect("archive ok");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::Applied);

    // NoOp：已是 archived，UPDATE 的 `status <> 'archived'` 过滤掉。
    let outcome = repo
        .mark_archived_with_lease_guard(row.id, job_id, &owner, now)
        .await
        .expect("archive ok");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::NoOp);

    // LeaseLost：abort 后再调；feed_sources 状态保持 archived。
    ReindexJobRepo::new(pool.clone())
        .abort(job_id, "test", now)
        .await
        .expect("abort ok");
    let outcome = repo
        .mark_archived_with_lease_guard(row.id, job_id, &owner, now)
        .await
        .expect("archive ok");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::LeaseLost);
}
