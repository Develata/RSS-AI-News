//! F15-7 W9-F4: SqliteReindexJobRepo::start_reindex_tx 单测。
//!
//! 锁定 reindex 启动入口的跨表事务语义：
//!   - 两条 INSERT 原子写入（rule_versions(status='pending') +
//!     reindex_jobs(state='pending')）；
//!   - 任一冲突整段回滚（rule_versions 不留"孤儿 pending"行，
//!     reindex_jobs 也不写入半成品行）；
//!   - F15-7 过渡原语 complete_without_claim 把 pending → completed，
//!     仅校验 state='pending'，**不**校验 lease_owner。

mod common;

use rss_ai_news_storage::{ReindexJobRepository, SqliteReindexJobRepo, StorageError};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::make_test_pool;

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("valid timestamp")
}

async fn count_rule_versions(pool: &SqlitePool, kind: &str, tag: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM rule_versions WHERE kind = ? AND version_tag = ?",
    )
    .bind(kind)
    .bind(tag)
    .fetch_one(pool)
    .await
    .expect("count rule_versions")
}

async fn count_reindex_jobs_for_target(pool: &SqlitePool, target: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reindex_jobs WHERE target = ?")
        .bind(target)
        .fetch_one(pool)
        .await
        .expect("count reindex_jobs")
}

async fn rule_status(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar::<_, String>("SELECT status FROM rule_versions WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("fetch rule_versions.status")
}

// --- start_reindex_tx happy path -------------------------------------------

#[tokio::test]
async fn writes_both_rows_atomically_with_pending_status() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteReindexJobRepo::new(pool.clone());

    let outcome = repo
        .start_reindex_tx(
            "reindex",
            "tag-link-hash-001",
            "first link-hash reindex",
            "sha-001",
            "link_hash",
            ts(0),
        )
        .await
        .expect("start_reindex_tx should succeed");

    assert!(outcome.rule_version_id > 0);
    assert!(outcome.job_id > 0);
    assert_eq!(
        rule_status(&pool, outcome.rule_version_id).await,
        "pending",
        "rule_versions 行必须以 status='pending' 写入"
    );

    let job = repo
        .find_by_id(outcome.job_id)
        .await
        .expect("find_by_id")
        .expect("job row exists");
    assert_eq!(job.target, "link_hash");
    assert_eq!(job.rule_version_id, outcome.rule_version_id);
    assert_eq!(job.state, "pending");
    assert_eq!(job.attempt_count, 0);
    assert!(job.lease_owner.is_none());
    assert!(job.lease_expires_at.is_none());
    assert!(job.last_processed_id.is_none());
}

#[tokio::test]
async fn distinct_targets_can_coexist_in_pending() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteReindexJobRepo::new(pool.clone());

    let a = repo
        .start_reindex_tx(
            "reindex",
            "tag-link-001",
            "link",
            "sha-link",
            "link_hash",
            ts(0),
        )
        .await
        .expect("first target");
    let b = repo
        .start_reindex_tx(
            "reindex",
            "tag-content-001",
            "content",
            "sha-content",
            "content_hash",
            ts(1),
        )
        .await
        .expect("second target");

    assert_ne!(a.rule_version_id, b.rule_version_id);
    assert_ne!(a.job_id, b.job_id);
    assert_eq!(count_reindex_jobs_for_target(&pool, "link_hash").await, 1);
    assert_eq!(
        count_reindex_jobs_for_target(&pool, "content_hash").await,
        1
    );
}

// --- start_reindex_tx rollback paths ---------------------------------------

#[tokio::test]
async fn rolls_back_when_target_already_has_active_job() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteReindexJobRepo::new(pool.clone());

    repo.start_reindex_tx(
        "reindex",
        "tag-first",
        "first",
        "sha-first",
        "link_hash",
        ts(0),
    )
    .await
    .expect("first start should succeed");

    let err = repo
        .start_reindex_tx(
            "reindex",
            "tag-second",
            "second",
            "sha-second",
            "link_hash",
            ts(1),
        )
        .await
        .expect_err("second start should hit partial unique on reindex_jobs.target");

    match err {
        StorageError::Conflict { ref table, .. } => {
            assert_eq!(table, "reindex_jobs", "冲突应归因到 reindex_jobs 表");
        }
        other => panic!("expected Conflict on reindex_jobs, got: {other:?}"),
    }

    // 关键回滚断言：rule_versions 的 'tag-second' 必须**不在**库里——
    // 反例（rule_versions 已 commit、reindex_jobs 才回滚）会泄露孤儿
    // pending 行，永远无法被 active_rule 选中。
    assert_eq!(
        count_rule_versions(&pool, "reindex", "tag-second").await,
        0,
        "rule_versions 'tag-second' 必须随 reindex_jobs 一起回滚"
    );
    assert_eq!(
        count_reindex_jobs_for_target(&pool, "link_hash").await,
        1,
        "reindex_jobs 仍只有第一行"
    );
}

#[tokio::test]
async fn rolls_back_when_rule_version_tag_collides() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteReindexJobRepo::new(pool.clone());

    // 先用 link_hash + tag-shared 占住 rule_versions(kind='reindex',
    // version_tag='tag-shared')。
    repo.start_reindex_tx(
        "reindex",
        "tag-shared",
        "first",
        "sha-shared",
        "link_hash",
        ts(0),
    )
    .await
    .expect("seed first reindex");
    repo.complete_without_claim(
        repo.find_active_by_target("link_hash")
            .await
            .unwrap()
            .unwrap()
            .id,
        ts(2),
    )
    .await
    .expect("clear partial unique on link_hash so target isn't the blocker");

    let err = repo
        .start_reindex_tx(
            "reindex",
            "tag-shared", // 命中 UNIQUE(kind, version_tag)
            "second",
            "sha-other",
            "content_hash",
            ts(3),
        )
        .await
        .expect_err("UNIQUE(kind, version_tag) should reject second insert");

    match err {
        StorageError::Conflict { ref table, .. } => {
            assert_eq!(table, "rule_versions", "冲突应归因到 rule_versions 表");
        }
        other => panic!("expected Conflict on rule_versions, got: {other:?}"),
    }

    // reindex_jobs 没新增 content_hash 行（rule_versions INSERT 失败时
    // reindex_jobs 那条 INSERT 根本没执行；事务回滚也覆盖到位）。
    assert_eq!(
        count_reindex_jobs_for_target(&pool, "content_hash").await,
        0,
        "reindex_jobs 不应留 content_hash 行"
    );
}

#[tokio::test]
async fn allows_restart_after_previous_terminal() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteReindexJobRepo::new(pool.clone());

    let first = repo
        .start_reindex_tx("reindex", "tag-r1", "r1", "sha-r1", "link_hash", ts(0))
        .await
        .expect("first start");

    // 把第一轮 job 推到 completed，partial unique 不再覆盖。
    assert!(
        repo.complete_without_claim(first.job_id, ts(5))
            .await
            .expect("complete first")
    );

    let second = repo
        .start_reindex_tx("reindex", "tag-r2", "r2", "sha-r2", "link_hash", ts(10))
        .await
        .expect("second start should succeed after first finalized");

    assert_ne!(first.job_id, second.job_id);
    assert_eq!(
        count_reindex_jobs_for_target(&pool, "link_hash").await,
        2,
        "两条历史 job 共存：一条 completed + 一条 pending"
    );
}

// --- complete_without_claim ------------------------------------------------

#[tokio::test]
async fn complete_without_claim_finalizes_pending_row_and_clears_lease_fields() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteReindexJobRepo::new(pool.clone());

    let outcome = repo
        .start_reindex_tx("reindex", "tag-c1", "c1", "sha-c1", "link_hash", ts(0))
        .await
        .expect("start");

    let updated = repo
        .complete_without_claim(outcome.job_id, ts(7))
        .await
        .expect("complete_without_claim");
    assert!(updated, "应返回 true 表示恰好更新一行");

    let job = repo
        .find_by_id(outcome.job_id)
        .await
        .expect("find")
        .expect("row");
    assert_eq!(job.state, "completed");
    assert_eq!(job.finished_at, Some(ts(7)));
    assert!(job.lease_owner.is_none());
    assert!(job.lease_expires_at.is_none());
}

#[tokio::test]
async fn complete_without_claim_no_ops_for_non_pending_state() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteReindexJobRepo::new(pool.clone());

    let outcome = repo
        .start_reindex_tx("reindex", "tag-c2", "c2", "sha-c2", "link_hash", ts(0))
        .await
        .expect("start");

    // 直接 SQL 把 state 改成 running，模拟"已被 worker claim"的状态。
    sqlx::query("UPDATE reindex_jobs SET state='running' WHERE id = ?")
        .bind(outcome.job_id)
        .execute(&pool)
        .await
        .expect("flip state to running");

    let updated = repo
        .complete_without_claim(outcome.job_id, ts(3))
        .await
        .expect("complete_without_claim");
    assert!(!updated, "running 态不应被 complete_without_claim 覆盖");

    let job = repo
        .find_by_id(outcome.job_id)
        .await
        .expect("find")
        .expect("row");
    assert_eq!(job.state, "running");
    assert!(job.finished_at.is_none());
}

#[tokio::test]
async fn complete_without_claim_no_ops_for_missing_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteReindexJobRepo::new(pool.clone());

    let updated = repo
        .complete_without_claim(9_999_999, ts(0))
        .await
        .expect("complete_without_claim");
    assert!(!updated, "缺失行应返 false 而非 panic");
}
