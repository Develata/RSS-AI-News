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
        .upsert_with_lease_guard(
            &src,
            job_id,
            &owner,
            OffsetDateTime::now_utc(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
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
        .upsert_with_lease_guard(
            &src,
            job_id,
            &owner,
            OffsetDateTime::now_utc(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
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
        .upsert_with_lease_guard(
            &src,
            job_id,
            "imposter-owner",
            OffsetDateTime::now_utc(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
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
    repo.upsert_with_lease_guard(
        &sample_source("ai", "main", config_id),
        job_id,
        &owner,
        now,
        now + time::Duration::hours(1),
    )
    .await
    .expect("upsert ok");
    let row = repo
        .find_by_keys("ai", "main")
        .await
        .expect("find ok")
        .expect("row exists");

    // Applied：active → archived。
    let outcome = repo
        .mark_archived_with_lease_guard(row.id, job_id, &owner, now, now + time::Duration::hours(1))
        .await
        .expect("archive ok");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::Applied);

    // NoOp：已是 archived，UPDATE 的 `status <> 'archived'` 过滤掉。
    let outcome = repo
        .mark_archived_with_lease_guard(row.id, job_id, &owner, now, now + time::Duration::hours(1))
        .await
        .expect("archive ok");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::NoOp);

    // LeaseLost：abort 后再调；feed_sources 状态保持 archived。
    ReindexJobRepo::new(pool.clone())
        .abort(job_id, "test", now)
        .await
        .expect("abort ok");
    let outcome = repo
        .mark_archived_with_lease_guard(row.id, job_id, &owner, now, now + time::Duration::hours(1))
        .await
        .expect("archive ok");
    assert_eq!(outcome, LeaseGuardedWriteOutcome::LeaseLost);
}

/// Runtime state the fetch path owns; config sync must not clobber it.
async fn set_runtime_state(pool: &SqlitePool, category: &str, source: &str) {
    sqlx::query(
        "UPDATE feed_sources SET etag = 'e1', last_modified = 'lm1', \
             last_fetched_at = '2026-11-20T10:00:00Z', last_success_at = '2026-11-20T10:00:00Z', \
             consecutive_failures = 3, last_error = 'boom', last_error_kind = 'http_5xx' \
         WHERE category_key = ? AND source_key = ?",
    )
    .bind(category)
    .bind(source)
    .execute(pool)
    .await
    .expect("runtime state");
}

/// A14: the categories reindex writes config columns only; fetch runtime
/// state survives unless the endpoint (feed_url / feed_kind) changed, the
/// same rule ingest's config sync applies.
#[tokio::test]
async fn config_upsert_keeps_runtime_state_unless_the_endpoint_changes() {
    let (_dir, pool) = make_test_pool().await;
    let (job_id, owner) = seed_running_job(&pool).await;
    let config_id = insert_rule(&pool, "config", "cfg-a14", "cfg-sha-a14").await;
    let repo = FeedSourceRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();
    let src = sample_source("ai", "main", config_id);
    assert_eq!(
        repo.upsert_config_with_lease_guard(
            &src,
            job_id,
            &owner,
            now,
            now + time::Duration::hours(1)
        )
        .await
        .unwrap(),
        LeaseGuardedWriteOutcome::Applied
    );
    set_runtime_state(&pool, "ai", "main").await;

    // Same endpoint, new display name / priority / status.
    let mut renamed = src.clone();
    renamed.display_name = "Renamed".to_string();
    renamed.priority = 7;
    renamed.status = FeedSourceStatus::Paused;
    repo.upsert_config_with_lease_guard(
        &renamed,
        job_id,
        &owner,
        now,
        now + time::Duration::hours(1),
    )
    .await
    .unwrap();
    let row = repo.find_by_keys("ai", "main").await.unwrap().unwrap();
    assert_eq!(
        (row.display_name.as_str(), row.priority, row.status),
        ("Renamed", 7, FeedSourceStatus::Paused)
    );
    assert_eq!(row.etag.as_deref(), Some("e1"));
    assert_eq!(row.last_modified.as_deref(), Some("lm1"));
    assert!(row.last_fetched_at.is_some() && row.last_success_at.is_some());
    assert_eq!(row.consecutive_failures, 3);
    assert_eq!(row.last_error_kind.as_deref(), Some("http_5xx"));

    // Endpoint change: validators and failure state belong to the old URL.
    let mut moved = renamed.clone();
    moved.feed_url = "https://example.com/new.xml".to_string();
    repo.upsert_config_with_lease_guard(
        &moved,
        job_id,
        &owner,
        now,
        now + time::Duration::hours(1),
    )
    .await
    .unwrap();
    let row = repo.find_by_keys("ai", "main").await.unwrap().unwrap();
    assert_eq!(row.feed_url, "https://example.com/new.xml");
    assert_eq!((row.etag, row.last_modified), (None, None));
    assert_eq!(row.consecutive_failures, 0);
    assert_eq!((row.last_error, row.last_error_kind), (None, None));
    assert!(row.last_success_at.is_some(), "history timestamps are kept");

    set_runtime_state(&pool, "ai", "main").await;
    let mut rekinded = moved.clone();
    rekinded.feed_kind = FeedKind::Atom;
    repo.upsert_config_with_lease_guard(
        &rekinded,
        job_id,
        &owner,
        now,
        now + time::Duration::hours(1),
    )
    .await
    .unwrap();
    let row = repo.find_by_keys("ai", "main").await.unwrap().unwrap();
    assert_eq!(row.feed_kind, FeedKind::Atom);
    assert_eq!(row.etag, None, "feed_kind change also resets");
}

#[tokio::test]
async fn config_upsert_without_lease_writes_nothing() {
    let (_dir, pool) = make_test_pool().await;
    let (job_id, _owner) = seed_running_job(&pool).await;
    let config_id = insert_rule(&pool, "config", "cfg-a14b", "cfg-sha-a14b").await;
    let repo = FeedSourceRepo::new(pool.clone());

    let outcome = repo
        .upsert_config_with_lease_guard(
            &sample_source("ai", "main", config_id),
            job_id,
            "imposter",
            OffsetDateTime::now_utc(),
            OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await
        .unwrap();

    assert_eq!(outcome, LeaseGuardedWriteOutcome::LeaseLost);
    assert_eq!(count_feed_sources(&pool).await, 0);
}

/// R2-S18: each guarded config write renews the job's lease, so a categories
/// sync that keeps progressing past its initial lease cannot be reclaimed.
#[tokio::test]
async fn guarded_config_writes_renew_the_job_lease() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_rule(&pool, "config", "cfg-renew", "sha-cfg-renew").await;
    let (job_id, owner) = seed_running_job(&pool).await;
    let jobs = ReindexJobRepo::new(pool.clone());
    let repo = FeedSourceRepo::new(pool.clone());
    let initial = jobs
        .find_by_id(job_id)
        .await
        .unwrap()
        .unwrap()
        .lease_expires_at
        .unwrap();

    // Two writes, the second after the initial lease would have expired.
    for (i, at) in [
        initial - Duration::seconds(10),
        initial + Duration::seconds(30),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            repo.upsert_config_with_lease_guard(
                &sample_source("ai", &format!("renew-{i}"), config_id),
                job_id,
                &owner,
                at - Duration::seconds(20),
                at + Duration::seconds(60),
            )
            .await
            .unwrap(),
            LeaseGuardedWriteOutcome::Applied
        );
    }

    // Past the initial lease, inside the renewed one: not reclaimable.
    assert_eq!(
        jobs.reclaim_expired_leases(initial + Duration::seconds(30))
            .await
            .unwrap(),
        0
    );
    let job = jobs.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.lease_owner.as_deref(), Some(owner.as_str()));
    assert_eq!(job.lease_expires_at, Some(initial + Duration::seconds(90)));
    assert_eq!(
        repo.mark_archived_with_lease_guard(
            repo.find_by_keys("ai", "renew-0")
                .await
                .unwrap()
                .unwrap()
                .id,
            job_id,
            &owner,
            initial + Duration::seconds(80),
            initial + Duration::seconds(200),
        )
        .await
        .unwrap(),
        LeaseGuardedWriteOutcome::Applied
    );
    let job = jobs.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.lease_expires_at, Some(initial + Duration::seconds(200)));
}
