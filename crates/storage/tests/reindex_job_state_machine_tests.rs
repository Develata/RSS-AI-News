//! F15-6 W9-F3 — reindex_job 进度推进 / 终态 / reclaim / 读路径单测。
//!
//! 一条测试锁定一条 transition 或一条失败回退路径，覆盖 state-machine §6.3
//! 中 job 已进入 running 之后的弧：
//!
//!   - `advance_checkpoint`：running → running 的 checkpoint；双 guard 拒绝
//!     wrong owner / non-running。
//!   - `assert_lease_held`（F15-fix2）：仅 lease guard 查询，不写
//!     last_processed_id；categories target 等无 checkpoint 语义的写路径
//!     用它做 per-write 守护。
//!   - `advance_to_completed`：running → completed；双 guard；写 finished_at、
//!     清 lease；**不**触跨表激活（那是 finish_reindex_tx 的事）。
//!   - `mark_failed`：running → failed；双 guard。
//!   - `abort`：pending / running → aborted；终态不再 abort。
//!   - `reclaim_expired_leases`：running → pending（lease 过期）；保留
//!     `last_processed_id` / `started_at` / `attempt_count`。
//!   - `list_running` / `find_by_id` / `find_active_by_target`：读路径。
//!
//! 进入 running 的领取路径（insert_pending / claim_pending / claim_by_id）见
//! `reindex_job_claim_tests.rs`；跨表 finalize TX 见
//! `reindex_job_finish_tx_tests.rs`。

mod common;

use rss_ai_news_storage::{ReindexJobRepo, ReindexJobRepository};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{insert_rule, make_test_pool};

/// 写入一个 rule_versions 行作为 FK target，返回 id。
async fn seed_rule_version(pool: &SqlitePool, tag: &str) -> i64 {
    insert_rule(pool, "extractor", tag, &format!("sha-{tag}")).await
}

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("valid timestamp")
}

// ---------------------------------------------------------------------------
// advance_checkpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advance_checkpoint_writes_progress_for_owning_worker() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let updated = repo
        .advance_checkpoint(job_id, "worker-a", 1234, ts(10))
        .await
        .expect("checkpoint ok");

    assert!(updated);
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.last_processed_id, Some(1234));
    assert_eq!(row.state, "running");
    assert_eq!(row.lease_owner.as_deref(), Some("worker-a"));
}

#[tokio::test]
async fn advance_checkpoint_rejects_wrong_owner() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let updated = repo
        .advance_checkpoint(job_id, "worker-b", 999, ts(10))
        .await
        .expect("checkpoint query ok");

    assert!(!updated, "lease_owner 不匹配必须拒写");
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert!(row.last_processed_id.is_none());
}

#[tokio::test]
async fn advance_checkpoint_rejects_when_not_running() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();

    let updated = repo
        .advance_checkpoint(job_id, "worker-a", 1, ts(1))
        .await
        .expect("checkpoint query ok");

    assert!(!updated, "pending 态不应被 checkpoint 推进");
}

// ---------------------------------------------------------------------------
// assert_lease_held (F15-fix2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn assert_lease_held_returns_true_for_owning_worker_in_running_state() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let held = repo
        .assert_lease_held(job_id, "worker-a", ts(10))
        .await
        .expect("assert_lease_held query ok");
    assert!(held, "running + 自己持有的 lease 必须命中");
    // updated_at 同步刷新（reclaim 巡检靠它判断 worker 活动）
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.updated_at, ts(10));
}

#[tokio::test]
async fn assert_lease_held_returns_false_for_wrong_owner() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let held = repo
        .assert_lease_held(job_id, "worker-b", ts(10))
        .await
        .expect("assert_lease_held query ok");
    assert!(!held, "wrong owner 必须返 false");
}

#[tokio::test]
async fn assert_lease_held_returns_false_after_abort_or_reclaim() {
    // abort 把 state→aborted；reclaim 把 state→pending；任一情况下原
    // worker 的 assert_lease_held 都应该返 false——这正是 reindex flow
    // 用它当 per-write guard 的依据（categories target 没有 checkpoint
    // 顺带 guard，要靠这个原语挡住 abort 后旧 worker 的覆盖写）。
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    // abort 路径
    repo.abort(job_id, "test reason", ts(8))
        .await
        .expect("abort ok");
    let held = repo
        .assert_lease_held(job_id, "worker-a", ts(10))
        .await
        .unwrap();
    assert!(!held, "abort 之后 assert_lease_held 必须返 false");
}

// ---------------------------------------------------------------------------
// advance_to_completed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advance_to_completed_finalizes_running_job_and_clears_lease() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let updated = repo
        .advance_to_completed(job_id, "worker-a", ts(100))
        .await
        .expect("complete ok");

    assert!(updated);
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "completed");
    assert_eq!(row.finished_at, Some(ts(100)));
    assert!(row.lease_owner.is_none());
    assert!(row.lease_expires_at.is_none());
    assert!(row.error.is_none());
}

#[tokio::test]
async fn advance_to_completed_rejects_wrong_owner() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let updated = repo
        .advance_to_completed(job_id, "worker-b", ts(100))
        .await
        .expect("query ok");

    assert!(!updated);
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(
        row.state, "running",
        "wrong owner 不得把 running 推到 completed"
    );
}

#[tokio::test]
async fn advance_to_completed_rejects_when_pending() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();

    let updated = repo
        .advance_to_completed(job_id, "worker-a", ts(100))
        .await
        .expect("query ok");

    assert!(
        !updated,
        "pending → completed 不是合法弧（必须先 claim 到 running）"
    );
}

// ---------------------------------------------------------------------------
// mark_failed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mark_failed_writes_error_and_clears_lease() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let updated = repo
        .mark_failed(job_id, "worker-a", "sha256 mismatch", ts(100))
        .await
        .expect("mark_failed ok");

    assert!(updated);
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "failed");
    assert_eq!(row.error.as_deref(), Some("sha256 mismatch"));
    assert_eq!(row.finished_at, Some(ts(100)));
    assert!(row.lease_owner.is_none());
}

#[tokio::test]
async fn mark_failed_rejects_wrong_owner() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let updated = repo
        .mark_failed(job_id, "worker-b", "oops", ts(100))
        .await
        .expect("query ok");

    assert!(!updated);
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "running");
    assert!(row.error.is_none());
}

// ---------------------------------------------------------------------------
// abort
// ---------------------------------------------------------------------------

#[tokio::test]
async fn abort_from_pending_succeeds() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();

    let updated = repo
        .abort(job_id, "user cancelled", ts(50))
        .await
        .expect("abort ok");

    assert!(updated);
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "aborted");
    assert_eq!(row.aborted_reason.as_deref(), Some("user cancelled"));
    assert_eq!(row.finished_at, Some(ts(50)));
}

#[tokio::test]
async fn abort_from_running_clears_lease() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let updated = repo
        .abort(job_id, "operator stop", ts(80))
        .await
        .expect("abort ok");

    assert!(updated);
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "aborted");
    assert_eq!(row.aborted_reason.as_deref(), Some("operator stop"));
    assert!(row.lease_owner.is_none());
    assert!(row.lease_expires_at.is_none());
}

#[tokio::test]
async fn abort_rejects_terminal_states() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();
    repo.advance_to_completed(job_id, "worker-a", ts(50))
        .await
        .unwrap();

    let updated = repo
        .abort(job_id, "too late", ts(60))
        .await
        .expect("abort query ok");

    assert!(!updated, "终态（completed）不应再被 abort 改写");
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "completed");
    assert!(row.aborted_reason.is_none());
}

// ---------------------------------------------------------------------------
// reclaim_expired_leases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reclaim_expired_leases_returns_zero_when_nothing_expired() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    repo.insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(200))
        .await
        .unwrap()
        .unwrap();

    let reclaimed = repo
        .reclaim_expired_leases(ts(100))
        .await
        .expect("reclaim ok");

    assert_eq!(reclaimed, 0);
}

#[tokio::test]
async fn reclaim_expired_leases_returns_running_to_pending_preserving_progress() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(30))
        .await
        .unwrap()
        .unwrap();
    repo.advance_checkpoint(job_id, "worker-a", 4242, ts(10))
        .await
        .unwrap();

    let reclaimed = repo
        .reclaim_expired_leases(ts(60))
        .await
        .expect("reclaim ok");

    assert_eq!(reclaimed, 1);
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "pending");
    assert!(row.lease_owner.is_none());
    assert!(row.lease_expires_at.is_none());
    assert_eq!(
        row.last_processed_id,
        Some(4242),
        "reclaim 必须保留 last_processed_id（断点续传）"
    );
    assert_eq!(row.started_at, Some(ts(5)), "reclaim 不改 started_at");
    assert_eq!(
        row.attempt_count, 1,
        "reclaim 不改 attempt_count（state-machine §2.3）"
    );
}

#[tokio::test]
async fn reclaim_expired_leases_only_touches_expired_rows() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_expired = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(1), ts(10))
        .await
        .unwrap()
        .unwrap();
    let job_fresh = repo
        .insert_pending("feed_entries", rule_id, ts(2))
        .await
        .unwrap();
    repo.claim_pending("worker-b", ts(3), ts(300))
        .await
        .unwrap()
        .unwrap();

    let reclaimed = repo
        .reclaim_expired_leases(ts(60))
        .await
        .expect("reclaim ok");

    assert_eq!(reclaimed, 1);
    let row_expired = repo.find_by_id(job_expired).await.unwrap().unwrap();
    let row_fresh = repo.find_by_id(job_fresh).await.unwrap().unwrap();
    assert_eq!(row_expired.state, "pending");
    assert_eq!(row_fresh.state, "running");
    assert_eq!(row_fresh.lease_owner.as_deref(), Some("worker-b"));
}

// ---------------------------------------------------------------------------
// 读路径：list_running / find_active_by_target
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_running_returns_pending_and_running_only() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    // pending
    let pending_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    // running
    let running_target = "feed_entries";
    let running_id = repo
        .insert_pending(running_target, rule_id, ts(1))
        .await
        .unwrap();
    // claim_pending 出队顺序按 (created_at ASC, id ASC)，因此只取第一个 pending
    // 进入 running；要让 running_id 变 running，需要先把 articles 也终态化或
    // 通过直接 SQL 把 running_id 推到 running。这里用直接 SQL，避免触发顺序。
    sqlx::query(
        "UPDATE reindex_jobs SET state='running', lease_owner='w', lease_expires_at=? WHERE id=?",
    )
    .bind(ts(500))
    .bind(running_id)
    .execute(&pool)
    .await
    .unwrap();
    // completed
    let completed_id = repo
        .insert_pending("publish", rule_id, ts(2))
        .await
        .unwrap();
    sqlx::query("UPDATE reindex_jobs SET state='completed', finished_at=? WHERE id=?")
        .bind(ts(3))
        .bind(completed_id)
        .execute(&pool)
        .await
        .unwrap();

    let running = repo.list_running().await.expect("list_running ok");

    let ids: Vec<i64> = running.iter().map(|r| r.id).collect();
    assert!(ids.contains(&pending_id));
    assert!(ids.contains(&running_id));
    assert!(!ids.contains(&completed_id));
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn find_active_by_target_returns_running_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();

    let found = repo
        .find_active_by_target("articles")
        .await
        .expect("query ok")
        .expect("active row exists");

    assert_eq!(found.id, job_id);
    assert_eq!(found.state, "running");
}

#[tokio::test]
async fn find_active_by_target_returns_none_when_all_jobs_terminal() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .unwrap();
    repo.claim_pending("worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();
    repo.advance_to_completed(job_id, "worker-a", ts(50))
        .await
        .unwrap();

    let found = repo
        .find_active_by_target("articles")
        .await
        .expect("query ok");

    assert!(
        found.is_none(),
        "completed/aborted/failed 终态行不应被 find_active_by_target 召回"
    );
}

#[tokio::test]
async fn find_by_id_returns_none_for_missing_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool);

    let row = repo.find_by_id(9999).await.expect("query ok");

    assert!(row.is_none());
}
