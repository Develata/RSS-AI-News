//! W17：`rotate_active_config` 的 PG 分支验证（docs/plan/16-config-versioning.md §4）。
//!
//! sqlite 版语义见 `rule_version_rotate_tests.rs`；本文件聚焦 PG 特有风险：
//! - `$N` 占位 SQL 真实执行（编译期不校验语法/类型映射）
//! - 并发轮换时 revive 路径命中 partial unique 23505 → 单次 retry
//!   （codex W16-fix1，`fe6c65b`）—— sqlite 写串行化下该 race 不存在，
//!   只能在 PG 上实证。
//!
//! 默认 `#[ignore]`，需要 docker；CI / 本地手跑加 `--include-ignored`。

mod common;

use std::sync::Arc;

use common::pg::make_pg_test_pool;
use rss_ai_news_storage::{ConfigRotation, RuleVersionRepo, RuleVersionRepository};
use sqlx::PgPool;
use time::OffsetDateTime;

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

async fn rotate(repo: &RuleVersionRepo, sha: &str) -> ConfigRotation {
    repo.rotate_active_config(sha, "pg test config", OffsetDateTime::now_utc())
        .await
        .expect("rotate should succeed")
}

/// (status, payload_sha256, version_tag, retired_at)
async fn rule_row(pool: &PgPool, id: i64) -> (String, String, String, Option<OffsetDateTime>) {
    sqlx::query_as(
        "SELECT status, payload_sha256, version_tag, retired_at FROM rule_versions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("rule row should exist")
}

async fn count_active_config(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions WHERE kind='config' AND status='active'")
        .fetch_one(pool)
        .await
        .expect("count should succeed")
}

/// 串行生命周期一次走完：空库 seed → 同 sha no-op → 漂移轮换 → 回滚复活。
/// 合并为单测试以摊薄 per-schema fixture 成本（~500ms/个）。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_rotate_seed_noop_drift_rollback_lifecycle() {
    let ctx = make_pg_test_pool().await;
    let repo = RuleVersionRepo::new_with_storage(ctx.storage_pool().clone());

    // 1) 空库 seed
    let first = rotate(&repo, SHA_A).await;
    let ConfigRotation::Rotated {
        new_id: a_id,
        demoted_id,
    } = first
    else {
        panic!("first rotate on empty schema should report Rotated, got {first:?}");
    };
    assert_eq!(demoted_id, None, "empty schema has no active row to demote");
    let (status, payload, tag, retired_at) = rule_row(ctx.pg_pool(), a_id).await;
    assert_eq!((status.as_str(), payload.as_str()), ("active", SHA_A));
    assert_eq!(tag, &SHA_A[..12], "version_tag should be sha prefix");
    assert_eq!(retired_at, None);

    // 2) 同 sha no-op
    let second = rotate(&repo, SHA_A).await;
    assert_eq!(second, ConfigRotation::NoChange { id: a_id });

    // 3) 漂移轮换：A superseded（带 retired_at），B active
    let third = rotate(&repo, SHA_B).await;
    let ConfigRotation::Rotated {
        new_id: b_id,
        demoted_id,
    } = third
    else {
        panic!("sha drift should rotate, got {third:?}");
    };
    assert_eq!(demoted_id, Some(a_id));
    let (a_status, _, _, a_retired) = rule_row(ctx.pg_pool(), a_id).await;
    assert_eq!(a_status, "superseded");
    assert!(a_retired.is_some(), "demoted row should carry retired_at");

    // 4) 回滚 A→B→A：复用原 A 行并清 retired_at，不插重复 sha 行
    let fourth = rotate(&repo, SHA_A).await;
    assert_eq!(
        fourth,
        ConfigRotation::Rotated {
            new_id: a_id,
            demoted_id: Some(b_id),
        },
        "rollback must revive the original A row"
    );
    let (a_status, _, _, a_retired) = rule_row(ctx.pg_pool(), a_id).await;
    assert_eq!(a_status, "active");
    assert_eq!(a_retired, None, "revival should clear retired_at");
    assert_eq!(count_active_config(ctx.pg_pool()).await, 1);

    ctx.cleanup().await;
}

/// D1 自愈：bootstrap placeholder 是首个 active config 行时，rotate 把它
/// 收编为 superseded —— 用户存量 PG 库下一次 CLI 启动走的就是这条路径。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_rotate_supersedes_bootstrap_placeholder() {
    let ctx = make_pg_test_pool().await;
    let repo = RuleVersionRepo::new_with_storage(ctx.storage_pool().clone());

    let placeholder_id = repo
        .active_rule_or_register(
            "config",
            "ingest-bootstrap",
            "auto-registered by ingest when no active config rule existed",
            "ingest-bootstrap",
        )
        .await
        .expect("placeholder seed should succeed");

    let outcome = rotate(&repo, SHA_A).await;

    let ConfigRotation::Rotated { new_id, demoted_id } = outcome else {
        panic!("placeholder active should be rotated out, got {outcome:?}");
    };
    assert_eq!(demoted_id, Some(placeholder_id));
    let (ph_status, _, _, _) = rule_row(ctx.pg_pool(), placeholder_id).await;
    assert_eq!(ph_status, "superseded", "placeholder self-heals on rotate");
    let (status, payload, _, _) = rule_row(ctx.pg_pool(), new_id).await;
    assert_eq!((status.as_str(), payload.as_str()), ("active", SHA_A));

    ctx.cleanup().await;
}

/// codex W16-fix1（`fe6c65b`）实证：并发轮换走 revive 路径时，落败方的
/// `REVIVE_CONFIG_VERSION_SQL` 在 partial unique `uq_rule_versions_kind_active`
/// 上命中 23505，必须被 `classify_db_error` 分类为 Conflict 并触发
/// `pg_rotate_active_config` 的单次 retry（last-writer-wins），而不是把
/// 启动打挂。
///
/// 构造：先串行 A→B→C 建出"A、B superseded + C active"的历史，再并发
/// rotate(A) / rotate(B)（双双走 revive 分支）。race 是否真实发生取决于
/// 调度时序，但不变量两种时序下都必须成立：
///   - 两次调用都 Ok（撞上 23505 时由 retry 吸收）
///   - 恰好 1 行 active（partial unique）
///   - 总行数仍是 3（revive 复用既有行，绝不插重复 sha）
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_rotate_concurrent_revive_last_writer_wins() {
    let ctx = make_pg_test_pool().await;
    let repo = Arc::new(RuleVersionRepo::new_with_storage(
        ctx.storage_pool().clone(),
    ));

    let a_id = rotate(&repo, SHA_A).await.active_id();
    let b_id = rotate(&repo, SHA_B).await.active_id();
    let _c_id = rotate(&repo, SHA_C).await.active_id();

    let repo_a = repo.clone();
    let repo_b = repo.clone();
    let now = OffsetDateTime::now_utc();
    let handle_a = tokio::spawn(async move {
        repo_a
            .rotate_active_config(SHA_A, "concurrent A", now)
            .await
    });
    let handle_b = tokio::spawn(async move {
        repo_b
            .rotate_active_config(SHA_B, "concurrent B", now)
            .await
    });

    let res_a = handle_a.await.expect("task A panicked");
    let res_b = handle_b.await.expect("task B panicked");
    let rot_a = res_a.expect("rotate(A) must succeed (retry should absorb 23505)");
    let rot_b = res_b.expect("rotate(B) must succeed (retry should absorb 23505)");

    assert_eq!(
        count_active_config(ctx.pg_pool()).await,
        1,
        "partial unique enforces exactly one active config row"
    );
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions WHERE kind='config'")
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(
        total, 3,
        "revive must reuse rows, never insert duplicate sha"
    );

    // 胜者是 A 或 B 之一；两个 rotation 报告的 active_id 也只能落在 {a_id, b_id}
    let active_payload: String = sqlx::query_scalar(
        "SELECT payload_sha256 FROM rule_versions WHERE kind='config' AND status='active'",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    assert!(
        active_payload == SHA_A || active_payload == SHA_B,
        "last writer must be one of the two concurrent rotations, got {active_payload}"
    );
    assert!(rot_a.active_id() == a_id && rot_b.active_id() == b_id);

    ctx.cleanup().await;
}
