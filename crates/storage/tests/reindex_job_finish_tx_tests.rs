//! F15-9 W9-F4 — reindex_job finish_reindex_tx 跨表 finalize 单测。
//!
//! 主路径：reindex_jobs running → completed + 旧 active 行 → superseded +
//! pending 行 → active，同一 sqlx 事务内完成。先 demote 后 promote 是必须的
//! 顺序约束（partial unique `uq_rule_versions_kind_active`(kind WHERE
//! status='active') 在每条 statement 后立即检查；反向顺序会在 promote 时与
//! 旧 active 行冲突）。
//!
//! 失败路径分两类：
//!   - lease guard 失败（job 已被 reclaim / 非 running / owner 不匹配）→
//!     整段回滚，返回 Outcome{ job_completed: false }，rule_versions 不变。
//!     调用方据此 warn 而非 Err。
//!   - 协议违例（rule_version_id 不是该 kind 的 pending 行）→ 返 Storage
//!     Error::Conflict，整段回滚，rule_versions 与 reindex_jobs **都**保持
//!     原状（包括旧 active 行不被 demote）。
//!
//! 进入 running 的领取路径见 `reindex_job_claim_tests.rs`；启动入口的跨表
//! TX（start_reindex_tx）见 `reindex_job_start_tx_tests.rs`。

mod common;

use rss_ai_news_storage::{ReindexJobRepo, ReindexJobRepository, StorageError};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::make_test_pool;

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("valid timestamp")
}

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
