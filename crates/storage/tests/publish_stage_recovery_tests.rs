//! Publish 阶段态崩溃恢复 + 耗尽收口不变量。
//!
//! Publish 是阶段驱动状态机：每阶段各自 claim 自己的输入态（pending /
//! snapshot_frozen / rendered / stored_local），advance 成功即清 lease。故进程
//! 在阶段之间崩溃，会把 record 留在某个中间态、lease 已清。本文件锁两条恢复
//! 不变量，防止"claim 只认 pending、一趟 advance 到底"这类改动悄悄引回死状态：
//!
//!   1. 中间态 + 清 lease + 预算未耗尽 → 只能被**对应阶段**的 claim 续跑（不串台），
//!      claim 后 attempt_count 自增。保证崩溃后没有"无人认领"的非终态。
//!   2. 中间态 + 过期 lease + 预算耗尽 → reclaim 清 lease 后，terminalize sweep
//!      把四个阶段态全部收口到 failed（既不无限重试，也不卡死）。

mod common;

use rss_ai_news_storage::{
    ClaimRequest, NewPublishRecord, PublishRecordRepo, PublishRecordRepository, build_owner_id,
    lease_expires_at,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_rule, make_test_pool};

const MAX_ATTEMPTS: u32 = 5;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp")
}

/// 建一条 publish_record 并强制其 state / attempt_count / lease 字段，
/// 模拟崩溃后留存的中间态行。`lease_until = None` 表示 lease 已清。
async fn seed_record(
    pool: &SqlitePool,
    key: &str,
    state: &str,
    attempt_count: i64,
    lease_until: Option<OffsetDateTime>,
) -> i64 {
    let render_id = insert_rule(
        pool,
        "render",
        &format!("render-{key}"),
        &format!("rsha-{key}"),
    )
    .await;
    let policy_id = insert_rule(
        pool,
        "selection_policy",
        &format!("policy-{key}"),
        &format!("psha-{key}"),
    )
    .await;
    let repo = PublishRecordRepo::new(pool.clone());
    let id = repo
        .create_if_new(&NewPublishRecord {
            idempotency_key: key.to_string(),
            category_key: "ai".to_string(),
            report_date: "2026-06-10".to_string(),
            target_timezone: "Asia/Shanghai".to_string(),
            render_version: render_id,
            selection_policy_version: policy_id,
            remote_target: None,
        })
        .await
        .expect("create should succeed")
        .expect("record should be new");
    let lease_owner = lease_until.map(|_| "dead-worker".to_string());
    sqlx::query(
        "UPDATE publish_records SET state = ?, attempt_count = ?, lease_owner = ?, lease_expires_at = ? WHERE id = ?",
    )
    .bind(state)
    .bind(attempt_count)
    .bind(lease_owner)
    .bind(lease_until)
    .bind(id)
    .execute(pool)
    .await
    .expect("seed update should succeed");
    id
}

async fn attempt_count(pool: &SqlitePool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT attempt_count FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("record should be readable")
}

async fn record_state(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("record should be readable")
}

fn claim_req(owner: &str) -> ClaimRequest {
    ClaimRequest {
        owner: owner.to_string(),
        now: now(),
        lease_expires_at: lease_expires_at(now(), Duration::seconds(30)),
        batch_size: 16,
        max_attempts: MAX_ATTEMPTS,
    }
}

#[tokio::test]
async fn each_cleared_lease_stage_state_is_claimable_only_by_its_stage() {
    let (_dir, pool) = make_test_pool().await;
    // 四个非终态行，attempt < max，lease 已清（崩溃在 advance 之后、下阶段 claim 之前）。
    let pending = seed_record(&pool, "p", "pending", 1, None).await;
    let frozen = seed_record(&pool, "f", "snapshot_frozen", 1, None).await;
    let rendered = seed_record(&pool, "r", "rendered", 1, None).await;
    let stored = seed_record(&pool, "s", "stored_local", 1, None).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let owner = build_owner_id();

    // 每个阶段 claim 只认自己的输入态——崩溃留存行被对应阶段精确续跑，互不串台。
    let ids = |rows: Vec<rss_ai_news_storage::ClaimedPublishRecord>| {
        rows.iter().map(|r| r.id).collect::<Vec<_>>()
    };
    let freeze = repo
        .claim_pending_for_freeze(&claim_req(&owner))
        .await
        .unwrap();
    assert_eq!(ids(freeze), vec![pending]);
    let render = repo
        .claim_frozen_for_render(&claim_req(&owner))
        .await
        .unwrap();
    assert_eq!(ids(render), vec![frozen]);
    let store = repo
        .claim_rendered_for_local_store(&claim_req(&owner))
        .await
        .unwrap();
    assert_eq!(ids(store), vec![rendered]);
    let remote = repo
        .claim_local_for_remote_publish(&claim_req(&owner))
        .await
        .unwrap();
    assert_eq!(ids(remote), vec![stored]);

    // claim 自增 attempt_count——崩溃恢复消耗共享预算，不会无限白嫖重试。
    assert_eq!(attempt_count(&pool, pending).await, 2);
    assert_eq!(attempt_count(&pool, stored).await, 2);
}

#[tokio::test]
async fn exhausted_stage_states_with_expired_lease_terminalize_after_reclaim() {
    let (_dir, pool) = make_test_pool().await;
    // 四个阶段态：预算耗尽 + 过期 lease（崩溃在持租阶段，预算已用完）。
    let expired = now() - Duration::seconds(1);
    let max = i64::from(MAX_ATTEMPTS);
    let pending = seed_record(&pool, "p", "pending", max, Some(expired)).await;
    let frozen = seed_record(&pool, "f", "snapshot_frozen", max, Some(expired)).await;
    let rendered = seed_record(&pool, "r", "rendered", max, Some(expired)).await;
    let stored = seed_record(&pool, "s", "stored_local", max, Some(expired)).await;
    let repo = PublishRecordRepo::new(pool.clone());

    // reclaim 清过期 lease（仅清 lease，不改 state）。
    let reclaimed = repo.reclaim_expired_leases(now()).await.unwrap();
    assert_eq!(reclaimed, 4);

    // sweep：预算耗尽 + lease 已清 → 四个阶段态全部收口 failed，无一卡死。
    let swept = repo
        .terminalize_exhausted(MAX_ATTEMPTS, now())
        .await
        .unwrap();
    assert_eq!(swept, 4);
    for id in [pending, frozen, rendered, stored] {
        assert_eq!(record_state(&pool, id).await, "failed");
    }
}
