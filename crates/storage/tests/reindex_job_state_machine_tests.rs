//! F15-6 W9-F3 — reindex_job 状态机单测。
//!
//! 一条测试锁定一条 transition 或一条失败回退路径，覆盖 state-machine §6.3
//! 的全部弧：
//!
//!   - `insert_pending`：(无) → pending；partial unique 拒绝重复未完成 job；
//!     允许同 target 在前一个 job 终态后再次 insert。
//!   - `claim_pending`：pending → running；started_at = COALESCE 在 reclaim 后
//!     不重置；空表返回 None；多 pending 时按 (created_at ASC, id ASC) 出队。
//!   - `advance_checkpoint`：running → running 的 checkpoint；双 guard 拒绝
//!     wrong owner / non-running。
//!   - `assert_lease_held`（F15-fix2）：仅 lease guard 查询，不写
//!     last_processed_id；categories target 等无 checkpoint 语义的写路径
//!     用它做 per-write 守护。
//!   - `advance_to_completed`：running → completed；双 guard；写 finished_at、
//!     清 lease；**不**触跨表激活（那是 F15-9 reindex finish flow 的事）。
//!   - `mark_failed`：running → failed；双 guard。
//!   - `abort`：pending / running → aborted；终态不再 abort。
//!   - `reclaim_expired_leases`：running → pending（lease 过期）；保留
//!     `last_processed_id` / `started_at` / `attempt_count`。
//!   - `list_running` / `find_by_id` / `find_active_by_target`：读路径。

mod common;

use rss_ai_news_storage::{ReindexJobRepo, ReindexJobRepository, StorageError};
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
// insert_pending
// ---------------------------------------------------------------------------

#[tokio::test]
async fn insert_pending_creates_row_in_pending_state_with_zero_attempts() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let now = ts(0);

    let job_id = repo
        .insert_pending("articles", rule_id, now)
        .await
        .expect("insert_pending should succeed");

    let row = repo
        .find_by_id(job_id)
        .await
        .expect("find_by_id ok")
        .expect("row exists");
    assert_eq!(row.target, "articles");
    assert_eq!(row.rule_version_id, rule_id);
    assert_eq!(row.state, "pending");
    assert_eq!(row.attempt_count, 0);
    assert!(row.lease_owner.is_none());
    assert!(row.lease_expires_at.is_none());
    assert!(row.started_at.is_none());
    assert!(row.finished_at.is_none());
    assert!(row.last_processed_id.is_none());
}

#[tokio::test]
async fn insert_pending_rejects_duplicate_active_job_for_same_target() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    repo.insert_pending("articles", rule_id, ts(0))
        .await
        .expect("first insert ok");

    let result = repo.insert_pending("articles", rule_id, ts(1)).await;

    assert!(
        matches!(result, Err(StorageError::Conflict { .. })),
        "partial unique 必须把第二个 active(pending/running) 同 target job 映射为 Conflict, got: {result:?}"
    );
}

#[tokio::test]
async fn insert_pending_allows_new_job_after_previous_terminal() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let first = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("first insert");
    let claimed = repo
        .claim_pending("worker-a", ts(1), ts(60))
        .await
        .expect("claim ok")
        .expect("got a job");
    assert_eq!(claimed.id, first);
    repo.advance_to_completed(first, "worker-a", ts(2))
        .await
        .expect("complete ok");

    let second = repo
        .insert_pending("articles", rule_id, ts(3))
        .await
        .expect("partial unique 不应阻止前一 job 终态后的新 insert");
    assert_ne!(second, first);
}

// ---------------------------------------------------------------------------
// claim_pending
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_pending_returns_none_when_no_pending_rows() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool);

    let claimed = repo
        .claim_pending("worker-a", ts(0), ts(60))
        .await
        .expect("claim ok");

    assert!(claimed.is_none());
}

#[tokio::test]
async fn claim_pending_transitions_to_running_and_writes_lease() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("insert");

    let claimed = repo
        .claim_pending("worker-a", ts(5), ts(65))
        .await
        .expect("claim ok")
        .expect("one row claimed");
    assert_eq!(claimed.id, job_id);
    assert_eq!(claimed.target, "articles");
    assert_eq!(claimed.attempt_count, 1);
    assert!(claimed.last_processed_id.is_none());

    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "running");
    assert_eq!(row.lease_owner.as_deref(), Some("worker-a"));
    assert_eq!(row.lease_expires_at, Some(ts(65)));
    assert_eq!(row.started_at, Some(ts(5)));
    assert_eq!(row.attempt_count, 1);
}

#[tokio::test]
async fn claim_pending_preserves_started_at_through_reclaim_cycle() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("insert");
    // 第一次 claim：started_at = ts(10)
    repo.claim_pending("worker-a", ts(10), ts(20))
        .await
        .unwrap()
        .unwrap();
    // lease 过期 → reclaim → 回到 pending
    let reclaimed = repo
        .reclaim_expired_leases(ts(30))
        .await
        .expect("reclaim ok");
    assert_eq!(reclaimed, 1);
    // 第二次 claim：started_at 仍是 ts(10)，attempt_count 累加到 2
    let claimed2 = repo
        .claim_pending("worker-b", ts(40), ts(100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed2.attempt_count, 2);

    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(
        row.started_at,
        Some(ts(10)),
        "COALESCE(started_at, :now) 必须保留首次 claim 时间"
    );
    assert_eq!(row.lease_owner.as_deref(), Some("worker-b"));
    assert_eq!(row.attempt_count, 2);
}

#[tokio::test]
async fn claim_pending_orders_by_created_at_then_id() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let earlier = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("first");
    let _later = repo
        .insert_pending("feed_entries", rule_id, ts(5))
        .await
        .expect("second");

    let claimed = repo
        .claim_pending("worker-a", ts(10), ts(70))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        claimed.id, earlier,
        "出队顺序必须按 (created_at ASC, id ASC) — 先 insert 的先 claim"
    );
}

// ---------------------------------------------------------------------------
// claim_by_id（F15-8 W9-F3）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_by_id_targets_specific_pending_even_when_older_exists() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let _older = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("older job");
    let target_job = repo
        .insert_pending("feed_entries", rule_id, ts(5))
        .await
        .expect("self job");

    // 关键反例：claim_pending 会按 (created_at ASC, id ASC) 取 _older；
    // reindex flow 必须能精确 claim 自己刚 INSERT 的那个 job。
    let claimed = repo
        .claim_by_id(target_job, "worker-self", ts(10), ts(70))
        .await
        .expect("claim_by_id ok")
        .expect("targeted row should claim");

    assert_eq!(claimed.id, target_job);
    assert_eq!(claimed.target, "feed_entries");
    assert_eq!(claimed.attempt_count, 1);

    // 旁证：older 仍是 pending、未被改动。
    let older_row = repo
        .find_active_by_target("articles")
        .await
        .unwrap()
        .expect("older pending still around");
    assert_eq!(older_row.state, "pending");
    assert!(older_row.lease_owner.is_none());
}

#[tokio::test]
async fn claim_by_id_writes_lease_started_at_and_increments_attempts() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("insert");

    let claimed = repo
        .claim_by_id(job_id, "worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.attempt_count, 1);

    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(row.state, "running");
    assert_eq!(row.lease_owner.as_deref(), Some("worker-a"));
    assert_eq!(row.lease_expires_at, Some(ts(65)));
    assert_eq!(row.started_at, Some(ts(5)));
    assert_eq!(row.attempt_count, 1);
}

#[tokio::test]
async fn claim_by_id_preserves_started_at_through_reclaim_cycle() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("insert");

    repo.claim_by_id(job_id, "worker-a", ts(10), ts(20))
        .await
        .unwrap()
        .unwrap();

    // lease 过期 → reclaim → 再 claim_by_id
    let reclaimed = repo.reclaim_expired_leases(ts(30)).await.unwrap();
    assert_eq!(reclaimed, 1);

    let again = repo
        .claim_by_id(job_id, "worker-b", ts(40), ts(100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.attempt_count, 2);

    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(
        row.started_at,
        Some(ts(10)),
        "COALESCE(started_at, :now) 必须保留首次 claim 时间"
    );
    assert_eq!(row.lease_owner.as_deref(), Some("worker-b"));
}

#[tokio::test]
async fn claim_by_id_rejects_when_already_running_with_valid_lease() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("insert");

    repo.claim_by_id(job_id, "worker-a", ts(5), ts(100))
        .await
        .unwrap()
        .unwrap();

    let second = repo
        .claim_by_id(job_id, "worker-b", ts(10), ts(110))
        .await
        .unwrap();
    assert!(second.is_none(), "lease 仍有效时不应被另一个 worker 抢走");
    let row = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(
        row.lease_owner.as_deref(),
        Some("worker-a"),
        "原 lease 不变"
    );
}

#[tokio::test]
async fn claim_by_id_returns_none_for_missing_id() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let result = repo
        .claim_by_id(9_999_999, "worker-a", ts(5), ts(65))
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn claim_by_id_returns_none_for_terminal_state() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let rule_id = seed_rule_version(&pool, "v1").await;
    let job_id = repo
        .insert_pending("articles", rule_id, ts(0))
        .await
        .expect("insert");

    repo.claim_by_id(job_id, "worker-a", ts(5), ts(65))
        .await
        .unwrap()
        .unwrap();
    assert!(
        repo.advance_to_completed(job_id, "worker-a", ts(10))
            .await
            .unwrap()
    );

    let result = repo
        .claim_by_id(job_id, "worker-b", ts(20), ts(80))
        .await
        .unwrap();
    assert!(result.is_none(), "完成态 job 不可重新 claim");
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

// ---------------------------------------------------------------------------
// finish_reindex_tx（F15-9 W9-F4 跨表 finalize TX）
// ---------------------------------------------------------------------------
//
// 主路径：reindex_jobs running → completed + 旧 active 行 → superseded +
// pending 行 → active，同一 sqlx 事务内完成。先 demote 后 promote 是必须的
// 顺序约束（partial unique `uq_rule_versions_kind_active`(kind WHERE
// status='active') 在每条 statement 后立即检查；反向顺序会在 promote 时与
// 旧 active 行冲突）。
//
// 失败路径分两类：
//   - lease guard 失败（job 已被 reclaim / 非 running / owner 不匹配）→
//     整段回滚，返回 Outcome{ job_completed: false }，rule_versions 不变。
//     调用方据此 warn 而非 Err。
//   - 协议违例（rule_version_id 不是该 kind 的 pending 行）→ 返 Storage
//     Error::Conflict，整段回滚，rule_versions 与 reindex_jobs **都**保持
//     原状（包括旧 active 行不被 demote）。

/// 直接 INSERT 一行 `status='active'` 的 rule_versions（用于构造"已有旧
/// active"场景；`common::insert_rule` 默认写 superseded）。
async fn seed_active_rule(pool: &SqlitePool, kind: &str, tag: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES (?, ?, 'active rule', ?, 'active') RETURNING id",
    )
    .bind(kind)
    .bind(tag)
    .bind(format!("sha-{tag}"))
    .fetch_one(pool)
    .await
    .expect("active rule should insert")
}

/// 用 `start_reindex_tx` + `claim_by_id` 构造一个 running 状态的 reindex_jobs
/// 行，并返回 `(rule_version_id, job_id)`。所有 finish_reindex_tx 测试都以
/// 此为起点。
async fn seed_running_reindex(
    repo: &ReindexJobRepo,
    tag: &str,
    target: &str,
    owner: &str,
    now: OffsetDateTime,
) -> (i64, i64) {
    let outcome = repo
        .start_reindex_tx("reindex", tag, "desc", &format!("sha-{tag}"), target, now)
        .await
        .expect("start_reindex_tx ok");
    repo.claim_by_id(
        outcome.job_id,
        owner,
        now,
        now + time::Duration::seconds(600),
    )
    .await
    .expect("claim_by_id ok")
    .expect("claim_by_id should succeed for freshly-inserted pending job");
    (outcome.rule_version_id, outcome.job_id)
}

async fn rule_status(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar::<_, String>("SELECT status FROM rule_versions WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("rule status query ok")
}

async fn rule_retired_at(pool: &SqlitePool, id: i64) -> Option<OffsetDateTime> {
    sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "SELECT retired_at FROM rule_versions WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("retired_at query ok")
}

#[tokio::test]
async fn finish_reindex_tx_promotes_pending_and_demotes_old_active() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let old_active_id = seed_active_rule(&pool, "reindex", "v-old").await;
    let (new_rule_id, job_id) =
        seed_running_reindex(&repo, "v-new", "articles", "worker-a", ts(10)).await;

    let outcome = repo
        .finish_reindex_tx(job_id, "worker-a", new_rule_id, "reindex", ts(99))
        .await
        .expect("finish_reindex_tx ok");

    assert!(outcome.job_completed, "lease guard 命中应当 job_completed");
    assert_eq!(outcome.demoted_rule_version_id, Some(old_active_id));

    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.state, "completed");
    assert!(job.finished_at.is_some());
    assert!(job.lease_owner.is_none());
    assert!(job.lease_expires_at.is_none());

    assert_eq!(rule_status(&pool, new_rule_id).await, "active");
    assert_eq!(rule_status(&pool, old_active_id).await, "superseded");
    assert_eq!(
        rule_retired_at(&pool, old_active_id).await,
        Some(ts(99)),
        "demote 同步写 retired_at"
    );
}

#[tokio::test]
async fn finish_reindex_tx_handles_first_version_without_demote() {
    // 该 kind 下尚无 active 行：demoted_rule_version_id = None，但 promote
    // 依然完成。
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let (new_rule_id, job_id) =
        seed_running_reindex(&repo, "v-first", "articles", "worker-a", ts(10)).await;

    let outcome = repo
        .finish_reindex_tx(job_id, "worker-a", new_rule_id, "reindex", ts(99))
        .await
        .expect("finish_reindex_tx ok");

    assert!(outcome.job_completed);
    assert!(
        outcome.demoted_rule_version_id.is_none(),
        "首版 reindex：该 kind 下无 active 行可 demote"
    );
    assert_eq!(rule_status(&pool, new_rule_id).await, "active");
    assert_eq!(
        repo.find_by_id(job_id).await.unwrap().unwrap().state,
        "completed"
    );
}

#[tokio::test]
async fn finish_reindex_tx_rolls_back_when_lease_owner_mismatches() {
    // lease guard 失败 → 整段回滚：rule_versions 不变（旧 active 仍 active /
    // pending 仍 pending），reindex_jobs 不变。
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let old_active_id = seed_active_rule(&pool, "reindex", "v-old").await;
    let (new_rule_id, job_id) =
        seed_running_reindex(&repo, "v-new", "articles", "worker-a", ts(10)).await;

    let outcome = repo
        .finish_reindex_tx(job_id, "intruder", new_rule_id, "reindex", ts(99))
        .await
        .expect("guard miss 不视为错误");

    assert!(
        !outcome.job_completed,
        "lease_owner 不匹配 → job_completed=false"
    );
    assert!(outcome.demoted_rule_version_id.is_none());

    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.state, "running", "reindex_jobs 不应被改动");
    assert_eq!(job.lease_owner.as_deref(), Some("worker-a"));
    assert_eq!(rule_status(&pool, old_active_id).await, "active");
    assert_eq!(
        rule_status(&pool, new_rule_id).await,
        "pending",
        "lease guard 失败时不应推进 rule_versions"
    );
}

#[tokio::test]
async fn finish_reindex_tx_rolls_back_when_job_not_running() {
    // job 处于 pending（未 claim）→ state guard 失败 → 整段回滚。
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let outcome_start = repo
        .start_reindex_tx("reindex", "v-new", "desc", "sha-v-new", "articles", ts(10))
        .await
        .expect("start_reindex_tx ok");
    let new_rule_id = outcome_start.rule_version_id;
    let job_id = outcome_start.job_id;

    let outcome = repo
        .finish_reindex_tx(job_id, "worker-a", new_rule_id, "reindex", ts(99))
        .await
        .expect("guard miss 不视为错误");

    assert!(!outcome.job_completed);
    assert_eq!(
        repo.find_by_id(job_id).await.unwrap().unwrap().state,
        "pending"
    );
    assert_eq!(rule_status(&pool, new_rule_id).await, "pending");
}

#[tokio::test]
async fn finish_reindex_tx_rejects_when_rule_version_already_consumed() {
    // 协议违例：rule_version_id 已经是 'active'（或其他非 pending 态）
    // → 返 Conflict；旧 active 行 **不应**被 demote（事务整段回滚）。
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let old_active_id = seed_active_rule(&pool, "reindex", "v-old").await;
    let (new_rule_id, job_id) =
        seed_running_reindex(&repo, "v-new", "articles", "worker-a", ts(10)).await;
    // 把 new_rule_id 强行从 pending 翻到 superseded，模拟 rule_versions 状态
    // 被外部破坏（注：直接绕过 partial unique，因 status='superseded' 不在
    // partial unique 的 WHERE 内）。
    sqlx::query("UPDATE rule_versions SET status='superseded' WHERE id = ?")
        .bind(new_rule_id)
        .execute(&pool)
        .await
        .unwrap();

    let result = repo
        .finish_reindex_tx(job_id, "worker-a", new_rule_id, "reindex", ts(99))
        .await;

    assert!(
        matches!(result, Err(StorageError::Conflict { ref table, .. }) if table == "rule_versions"),
        "rule_version_id 非 pending 应返 StorageError::Conflict, got: {result:?}"
    );
    // 关键反例：lease guard 已通过（reindex_jobs 那条 UPDATE 走过），但因 promote
    // 失败整段回滚，**旧 active 不能被 demote**，reindex_jobs 状态保持 running。
    assert_eq!(
        rule_status(&pool, old_active_id).await,
        "active",
        "promote 失败时 demote 也应被整段回滚"
    );
    assert_eq!(
        repo.find_by_id(job_id).await.unwrap().unwrap().state,
        "running",
        "reindex_jobs 行也应整段回滚"
    );
}

#[tokio::test]
async fn finish_reindex_tx_preserves_other_kinds_active_rows() {
    // 仅 demote 同 kind 的 active 行：其他 kind 的 active 行不动。
    let (_dir, pool) = make_test_pool().await;
    let repo = ReindexJobRepo::new(pool.clone());
    let other_active = seed_active_rule(&pool, "extractor", "v-extractor-active").await;
    let reindex_old_active = seed_active_rule(&pool, "reindex", "v-old").await;
    let (new_rule_id, job_id) =
        seed_running_reindex(&repo, "v-new", "articles", "worker-a", ts(10)).await;

    let outcome = repo
        .finish_reindex_tx(job_id, "worker-a", new_rule_id, "reindex", ts(99))
        .await
        .expect("finish_reindex_tx ok");

    assert_eq!(outcome.demoted_rule_version_id, Some(reindex_old_active));
    assert_eq!(
        rule_status(&pool, other_active).await,
        "active",
        "其他 kind 的 active 行不应被 demote"
    );
    assert!(rule_retired_at(&pool, other_active).await.is_none());
}
