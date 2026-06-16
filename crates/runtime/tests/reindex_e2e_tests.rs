//! W9c F15-12 — reindex 流水线端到端 8 用例集。
//!
//! 覆盖 reindex 在 F15-7..F15-11 全部 lease/跨表 TX 原语接入后的端到端行为
//! 合同：幂等 / 批次边界 / crash recovery / 隔离 / 并发拒绝 / failure 不污染
//! active / 部分失败 continue / dry-run 不污染 rule_versions 链。
//!
//! 各原语（三 target 重算 / jobs finalize / 跨表 finish TX / abort / dry-run）
//! 的细粒度单测见 `reindex_tests.rs`。

mod common;

use std::sync::Arc;

use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_runtime::{ReindexFlow, ReindexOptions, ReindexTarget};
use rss_ai_news_storage::ReindexJobRepository;
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

#[tokio::test]
async fn reindex_link_hash_second_run_with_all_unchanged_still_rotates_rule_versions() {
    // 幂等：第一次 reindex 把 wrong link_hash 推到正确值；第二次 reindex 在
    // 完全没有数据变更的前提下扫描，unchanged=1 / updated=0；但
    // rule_versions 链仍按 §6.3 推进——上一版 demote 到 superseded、新一版
    // promote 到 active（partial unique `uq_rule_versions_kind_active` 保证
    // 同时刻只有一行 active）。
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "idem",
        "https://example.com/feed.xml",
    )
    .await;
    common::seed_pending_fetch_entry(&pool, source, "idem-1", "wrong", None).await;

    let first = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert_eq!(first.updated, 1);

    let second = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert_eq!(second.scanned, 1);
    assert_eq!(second.unchanged, 1, "数据已稳态，第二次应全部 unchanged");
    assert_eq!(second.updated, 0);

    let first_status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(first.new_rule_version_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let second_status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(second.new_rule_version_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        first_status, "superseded",
        "幂等也要正常 demote 上一版——rule_versions 链与数据变化解耦"
    );
    assert_eq!(second_status, "active");

    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM rule_versions WHERE kind='reindex' AND status='active'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_count, 1, "同一时刻只能一行 active");
}

#[tokio::test]
async fn reindex_link_hash_batch_size_one_processes_all_rows_and_checkpoints_last_id() {
    // 批次边界：batch_size=1 + 3 行 → 3 个批次都被处理；最终
    // last_processed_id = 末行 id（每批末尾 checkpoint 推进，循环退出条件
    // 由 list_for_link_hash_reindex 返空驱动）。
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "batch",
        "https://example.com/feed.xml",
    )
    .await;
    common::seed_pending_fetch_entry(&pool, source, "b1", "wrong", None).await;
    common::seed_pending_fetch_entry(&pool, source, "b2", "wrong", None).await;
    let last_id = common::seed_pending_fetch_entry(&pool, source, "b3", "wrong", None).await;

    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 1))
        .await
        .unwrap();
    assert_eq!(summary.scanned, 3);
    assert_eq!(summary.updated, 3);

    let last_processed: Option<i64> =
        sqlx::query_scalar("SELECT last_processed_id FROM reindex_jobs WHERE id = ?")
            .bind(summary.reindex_job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        last_processed,
        Some(last_id),
        "checkpoint 必须推进到末行 id（覆盖所有批次的 advance_checkpoint）"
    );
}

#[tokio::test]
async fn reindex_lease_reclaim_preserves_checkpoint_and_started_at_for_resume() {
    // crash recovery 原语：state-machine §2.3 规定 lease 过期被 reclaim 后
    // job `running → pending`、清 lease、**保留** last_processed_id 与
    // started_at、**不**动 attempt_count。新 worker claim_by_id 续作时
    // attempt_count += 1，last_processed_id 透传，从断点续作。
    let (_dir, pool) = common::make_test_pool().await;
    let repo = rss_ai_news_storage::ReindexJobRepo::new(pool.clone());

    let started = OffsetDateTime::now_utc();
    let outcome = repo
        .start_reindex_tx(
            "reindex",
            "tag-resume",
            "desc",
            "sha-resume",
            "link_hash",
            started,
        )
        .await
        .unwrap();
    let job_id = outcome.job_id;
    let lease_expires = started + Duration::seconds(1);
    repo.claim_by_id(job_id, "worker-a", started, lease_expires)
        .await
        .unwrap()
        .unwrap();
    repo.advance_checkpoint(job_id, "worker-a", 100, started)
        .await
        .unwrap();

    let after_expire = started + Duration::seconds(2);
    let reclaimed = repo.reclaim_expired_leases(after_expire).await.unwrap();
    assert_eq!(reclaimed, 1, "lease 已过期，必须被 reclaim 1 行");

    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.state, "pending", "reclaim 应把 running 推回 pending");
    assert_eq!(
        job.last_processed_id,
        Some(100),
        "checkpoint 必须保留——这是 resume 的基础"
    );
    assert!(job.started_at.is_some(), "started_at 必须保留");
    assert!(job.lease_owner.is_none(), "lease_owner 必须清空");
    assert!(job.lease_expires_at.is_none(), "lease_expires_at 必须清空");
    assert_eq!(job.attempt_count, 1, "reclaim 不动 attempt_count");

    let claimed = repo
        .claim_by_id(
            job_id,
            "worker-b",
            after_expire + Duration::seconds(1),
            after_expire + Duration::seconds(600),
        )
        .await
        .unwrap()
        .expect("reclaim 后 claim_by_id 必须成功");
    assert_eq!(
        claimed.attempt_count, 2,
        "claim_by_id 在续作时 attempt_count += 1"
    );
    assert_eq!(
        claimed.last_processed_id,
        Some(100),
        "新 worker 续作时必须看到断点 last_processed_id"
    );
}

#[tokio::test]
async fn reindex_link_hash_does_not_modify_articles_or_content_hashes() {
    // 隔离：LinkHash target 只接 feed_entries.link_hash；articles 表的
    // content_hash / body_text / title 必须完全不变。锁定 cli-semantics
    // §4.8 line 285 三 target 互不交叉的边界。
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "iso",
        "https://example.com/feed.xml",
    )
    .await;
    common::seed_pending_fetch_entry(&pool, source, "iso-entry", "wrong-link-hash", None).await;
    let article_id =
        common::seed_persisted_article(&pool, "wrong-content-hash", "Original", "body text").await;

    let pre: (String, String, String) =
        sqlx::query_as("SELECT content_hash, title, body_text FROM articles WHERE id = ?")
            .bind(article_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert!(summary.updated >= 1, "feed_entries 应当被 LinkHash 推进");

    let post: (String, String, String) =
        sqlx::query_as("SELECT content_hash, title, body_text FROM articles WHERE id = ?")
            .bind(article_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        pre, post,
        "LinkHash target 必须与 articles 表完全隔离——三 target 不交叉"
    );
}

#[tokio::test]
async fn reindex_second_start_for_same_target_rejected_by_partial_unique() {
    // 并发拒绝：同 target 已有 pending/running job 时，第二个 start_reindex_tx
    // 必须被 `uq_reindex_jobs_target_active` 拒绝，并且整段 TX 回滚——
    // rule_versions 不能留孤儿 pending 行（F15-7 的核心不变量）。
    let (_dir, pool) = common::make_test_pool().await;
    let repo = rss_ai_news_storage::ReindexJobRepo::new(pool.clone());

    let now = OffsetDateTime::now_utc();
    repo.start_reindex_tx("reindex", "tag-1", "desc-1", "sha-1", "link_hash", now)
        .await
        .expect("第一次 start_reindex_tx 应成功");

    let err = repo
        .start_reindex_tx("reindex", "tag-2", "desc-2", "sha-2", "link_hash", now)
        .await
        .expect_err("partial unique 必须拒绝同 target 的第二个 pending/running");
    match err {
        rss_ai_news_storage::StorageError::Conflict { table, .. } => {
            assert_eq!(
                table, "reindex_jobs",
                "Conflict.table 应锁定到 reindex_jobs（partial unique 命中点）"
            );
        }
        other => panic!("期待 Conflict {{ table: reindex_jobs }}，实际：{other:?}"),
    }

    let jobs_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM reindex_jobs WHERE target='link_hash' AND state='pending'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs_count, 1, "第一行 pending job 必须保留");
    let rules_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions WHERE kind='reindex'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        rules_count, 1,
        "失败 TX 必须连带 rule_versions 一起回滚——不留孤儿 pending 行（F15-7 不变量）"
    );
}

#[tokio::test]
async fn reindex_mark_failed_keeps_old_active_and_pending_new_rule_version() {
    // failure 不污染 active：第一次 reindex 成功 → rule_v1 active。第二次
    // reindex 启动（start_reindex_tx 写 rule_v2 pending + job 行），claim 后
    // mark_failed。此时 rule_v1 必须仍是 active（finish_reindex_tx 才走
    // demote/promote），rule_v2 仍是 pending（管理员决定后续清理）。
    let (_dir, pool) = common::make_test_pool().await;

    let first = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    let first_active_id = first.new_rule_version_id;

    let repo = rss_ai_news_storage::ReindexJobRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();
    let started = repo
        .start_reindex_tx(
            "reindex",
            "tag-failpath",
            "desc",
            "sha-failpath",
            "categories",
            now,
        )
        .await
        .unwrap();
    let owner = "worker-fail";
    repo.claim_by_id(started.job_id, owner, now, now + Duration::seconds(600))
        .await
        .unwrap()
        .unwrap();
    let marked = repo
        .mark_failed(
            started.job_id,
            owner,
            "synthetic",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert!(marked, "running + lease guard 命中 → mark_failed 必须成功");

    let first_status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(first_active_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        first_status, "active",
        "mark_failed 不应触发 demote——active 链不受失败 reindex 污染"
    );

    let new_status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(started.rule_version_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        new_status, "pending",
        "失败 reindex 的 rule_versions 行保持 pending，由管理员决定清理"
    );

    let job = repo.find_by_id(started.job_id).await.unwrap().unwrap();
    assert_eq!(job.state, "failed");
    assert_eq!(job.error.as_deref(), Some("synthetic"));
    assert!(job.finished_at.is_some());
    assert!(job.lease_owner.is_none(), "终态必须清 lease_owner");
}

#[tokio::test]
async fn reindex_link_hash_partial_failure_continues_processing_remaining_rows() {
    // 部分失败 continue：中间一行 normalized_link 非法 → errors+=1 + continue；
    // 前后两行正常 update。errors 不打断批次循环，summary 综合三类计数。
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "partial",
        "https://example.com/feed.xml",
    )
    .await;
    common::seed_pending_fetch_entry(&pool, source, "p1", "wrong", None).await;
    let bad_id = common::seed_pending_fetch_entry(&pool, source, "p2", "wrong", None).await;
    common::seed_pending_fetch_entry(&pool, source, "p3", "wrong", None).await;
    sqlx::query("UPDATE feed_entries SET normalized_link = 'not a url' WHERE id = ?")
        .bind(bad_id)
        .execute(&pool)
        .await
        .unwrap();

    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert_eq!(summary.scanned, 3, "三行都被扫到（错误行也计 scanned）");
    assert_eq!(summary.updated, 2, "首尾两行正常 update");
    assert_eq!(summary.errors, 1, "中间一行计入 errors 但不打断循环");

    let bad_link_hash: String =
        sqlx::query_scalar("SELECT link_hash FROM feed_entries WHERE id = ?")
            .bind(bad_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(bad_link_hash, "wrong", "错误行的 link_hash 不应被改写");

    // partial failure 不阻止 finish_reindex_tx 走 promote 路径——这是
    // cli-semantics §4.8 line 325 的契约：reindex run 在有 errors 的情况下
    // 仍然完成 rule_versions 升级（管理员通过 errors 计数决策）。
    let status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(summary.new_rule_version_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        status, "active",
        "有 errors 也仍 promote 到 active；管理员据 errors 计数决策回滚"
    );
}

#[tokio::test]
async fn reindex_dry_run_then_real_run_promotes_without_polluting_rule_versions_chain() {
    // dry-run 链路隔离：dry-run 后 rule_versions(kind='reindex') 必须为 0 行；
    // 紧随其后的 real run 进入"首版 promote"路径（demoted_rule_version_id=None）
    // 而非"误把 dry-run 的孤儿 pending 行 demote"。
    let (_dir, pool) = common::make_test_pool().await;

    let dry = reindex(&pool)
        .dry_run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert_eq!(dry.new_rule_version_id, 0);
    assert_eq!(dry.reindex_job_id, 0);
    let pre_rules: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions WHERE kind='reindex'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pre_rules, 0, "dry-run 不应在 rule_versions 留下任何行");
    let pre_jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reindex_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pre_jobs, 0, "dry-run 不应创建 reindex_jobs 行");

    let real = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert!(real.new_rule_version_id > 0);
    let status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(real.new_rule_version_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "active");

    // 全表只剩 1 行 reindex rule（dry-run + real run 只有 real 写库）。
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions WHERE kind='reindex'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        total, 1,
        "dry-run + 1 次 real run 后 rule_versions 应仅有 1 行（real run 的 active）"
    );
}

fn reindex(pool: &SqlitePool) -> ReindexFlow {
    ReindexFlow::new(Arc::new(common::full_context(
        "reindex",
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Always, 1)),
        Arc::new(common::DummyFeedFetcher),
    )))
}

fn reindex_opts(target: ReindexTarget, batch_size: u32) -> ReindexOptions {
    ReindexOptions {
        target,
        batch_size,
        categories: vec![common::category_with_sources(&["new-a", "new-b"])],
        new_rule_version_tag: format!("tag-{}", OffsetDateTime::now_utc().unix_timestamp_nanos()),
        new_rule_version_description: "test".to_string(),
        new_rule_version_sha256: "sha".to_string(),
    }
}
