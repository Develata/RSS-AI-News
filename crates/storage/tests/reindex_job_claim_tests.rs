//! F15-6/F15-8 W9-F3 — reindex_job 进入 running 的领取路径单测。
//!
//! 覆盖 state-machine §6.3 中"把 job 推到 running"的弧：
//!
//!   - `insert_pending`：(无) → pending；partial unique 拒绝重复未完成 job；
//!     允许同 target 在前一个 job 终态后再次 insert。
//!   - `claim_pending`：pending → running；started_at = COALESCE 在 reclaim 后
//!     不重置；空表返回 None；多 pending 时按 (created_at ASC, id ASC) 出队。
//!   - `claim_by_id`：精确领取指定 job（即便有更早的 pending）；双 guard 拒绝
//!     已 running / 终态 / 缺失行。
//!
//! 领取后的进度推进 / 终态 / reclaim / 读路径见
//! `reindex_job_state_machine_tests.rs`；跨表 finalize TX 见
//! `reindex_job_finish_tx_tests.rs`。

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
