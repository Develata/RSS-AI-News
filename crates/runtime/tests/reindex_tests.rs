//! W9c F15-8..F15-10 — reindex flow 各原语单测。
//!
//!   - 三 target 重算：link_hash / content_hash / categories（含 config_version
//!     必须指向 kind='config' 行的 F15-fix3 反例）。
//!   - lease 驱动的 reindex_jobs 行 finalize（含无 checkpoint 的 categories）。
//!   - 跨表 finish TX 把 rule_versions 推到 active / demote 上一版。
//!   - abort 语义 + dry-run 高保真计数且不写库。
//!
//! 端到端 8 用例集（幂等 / 批次边界 / crash recovery / 隔离 / 并发拒绝 /
//! 失败不污染 active / 部分失败 continue / dry-run 链路）见
//! `reindex_e2e_tests.rs`。

mod common;

use std::sync::Arc;

use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_runtime::{ReindexFlow, ReindexOptions, ReindexTarget};
use rss_ai_news_storage::ReindexJobRepository;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

#[tokio::test]
async fn reindex_link_hash_recomputes_changed_rows() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "l1",
        "https://example.com/feed.xml",
    )
    .await;
    common::seed_pending_fetch_entry(&pool, source, "l1", "wrong", None).await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert_eq!(summary.updated, 1);
}

#[tokio::test]
async fn reindex_link_hash_unchanged_rows_counted() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "l2",
        "https://example.com/feed.xml",
    )
    .await;
    let normalized =
        rss_ai_news_domain::link_normalizer::normalize_link("https://example.com/l2").unwrap();
    common::seed_pending_fetch_entry(&pool, source, "l2", &normalized.link_hash, None).await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert_eq!(summary.unchanged, 1);
}

#[tokio::test]
async fn reindex_link_hash_invalid_url_counted_errors() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "l3",
        "https://example.com/feed.xml",
    )
    .await;
    let id = common::seed_pending_fetch_entry(&pool, source, "l3", "wrong", None).await;
    sqlx::query("UPDATE feed_entries SET normalized_link = 'not a url' WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert_eq!(summary.errors, 1);
}

#[tokio::test]
async fn reindex_content_hash_updates_when_body_text_diff() {
    let (_dir, pool) = common::make_test_pool().await;
    common::seed_persisted_article(&pool, "old-hash", "T", "new body").await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::ContentHash, 10))
        .await
        .unwrap();
    assert_eq!(summary.updated, 1);
}

#[tokio::test]
async fn reindex_content_hash_skips_unique_conflict() {
    let (_dir, pool) = common::make_test_pool().await;
    let body = "same body";
    let hash = sha256_hex(body.as_bytes());
    common::seed_persisted_article(&pool, &hash, "A", body).await;
    common::seed_persisted_article(&pool, "wrong-hash", "B", body).await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::ContentHash, 10))
        .await
        .unwrap();
    assert_eq!(summary.conflict_skipped, 1);
}

#[tokio::test]
async fn reindex_content_hash_unchanged_when_hash_matches() {
    let (_dir, pool) = common::make_test_pool().await;
    let body = "stable body";
    common::seed_persisted_article(&pool, &sha256_hex(body.as_bytes()), "A", body).await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::ContentHash, 10))
        .await
        .unwrap();
    assert_eq!(summary.unchanged, 1);
}

#[tokio::test]
async fn reindex_categories_inserts_new_sources() {
    let (_dir, pool) = common::make_test_pool().await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert_eq!(summary.updated, 2);
}

#[tokio::test]
async fn reindex_categories_archives_obsolete_sources() {
    let (_dir, pool) = common::make_test_pool().await;
    let cfg = common::insert_config_rule(&pool).await;
    common::insert_source(&pool, cfg, "obsolete", "https://example.com/old.xml").await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert_eq!(summary.archived, 1);
}

#[tokio::test]
async fn reindex_categories_second_run_archives_nothing() {
    let (_dir, pool) = common::make_test_pool().await;
    reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert_eq!(summary.archived, 0);
}

#[tokio::test]
async fn reindex_categories_writes_config_kind_id_into_feed_sources_config_version() {
    // F15-fix3：feed_sources.config_version 必须指向 `kind='config'` 行，
    // **不**能指向 reindex 自己创建的 `kind='reindex'` 行。
    let (_dir, pool) = common::make_test_pool().await;

    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert!(summary.updated > 0);

    // 收集所有刚 upsert 的 feed_sources 的 config_version → 反查 kind
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT rv.kind
         FROM feed_sources fs
         JOIN rule_versions rv ON rv.id = fs.config_version
         ORDER BY fs.id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        !kinds.is_empty(),
        "Categories reindex 应当产出至少一行 feed_sources"
    );
    for kind in &kinds {
        assert_eq!(
            kind, "config",
            "feed_sources.config_version 必须指向 kind='config' 行，实际：{kind}"
        );
    }
    // 而 reindex flow 自己创建的 kind='reindex' 行也确实存在（独立审计）
    let reindex_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions WHERE kind = 'reindex'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(reindex_count >= 1);
}

// --- F15-8 W9-F3: lease-driven reindex_jobs 行为锁定 -----------------------

#[tokio::test]
async fn reindex_link_hash_finalizes_reindex_jobs_row() {
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "lh-final",
        "https://example.com/feed.xml",
    )
    .await;
    let entry_id = common::seed_pending_fetch_entry(&pool, source, "lh-final", "wrong", None).await;

    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert_eq!(summary.updated, 1);
    assert!(summary.reindex_job_id > 0);

    let row = sqlx::query_as::<_, (String, i64, Option<i64>, Option<String>, Option<String>)>(
        "SELECT state, attempt_count, last_processed_id, lease_owner, finished_at
         FROM reindex_jobs WHERE id = ?",
    )
    .bind(summary.reindex_job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (state, attempt_count, last_processed_id, lease_owner, finished_at) = row;
    assert_eq!(state, "completed");
    assert_eq!(attempt_count, 1, "claim_by_id 应当只加 1");
    assert_eq!(
        last_processed_id,
        Some(entry_id),
        "advance_checkpoint 应把 last_processed_id 推到末行 id"
    );
    assert!(lease_owner.is_none(), "终态应清 lease_owner");
    assert!(finished_at.is_some(), "finished_at 必须落地");
}

#[tokio::test]
async fn reindex_categories_finalizes_reindex_jobs_row_without_checkpoint() {
    let (_dir, pool) = common::make_test_pool().await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert!(summary.updated > 0);
    assert!(summary.reindex_job_id > 0);

    let (state, last_processed_id): (String, Option<i64>) =
        sqlx::query_as("SELECT state, last_processed_id FROM reindex_jobs WHERE id = ?")
            .bind(summary.reindex_job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "completed");
    assert!(
        last_processed_id.is_none(),
        "Categories 无 after_id 分页，不应写 last_processed_id"
    );
}

// --- F15-9 W9-F4: 跨表 finish TX 端到端语义 -------------------------------

#[tokio::test]
async fn reindex_promotes_rule_version_to_active_on_completion() {
    // 首版 reindex（该 kind 下无 active 行）：start_reindex_tx 写 pending →
    // finish_reindex_tx 推到 active，retired_at 仍为 NULL。
    let (_dir, pool) = common::make_test_pool().await;
    let summary = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();

    let (status, retired_at): (String, Option<OffsetDateTime>) =
        sqlx::query_as("SELECT status, retired_at FROM rule_versions WHERE id = ?")
            .bind(summary.new_rule_version_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        status, "active",
        "首版 reindex 完成后 rule_versions 应推到 active"
    );
    assert!(retired_at.is_none(), "active 行 retired_at 必须为 NULL");
}

#[tokio::test]
async fn reindex_demotes_previous_active_rule_version_on_second_run() {
    // 第二次 reindex 完成时：第一次的 rule_versions 行从 active 降到
    // superseded 并写 retired_at；新行进 active。partial unique
    // `uq_rule_versions_kind_active` 不冲突的前提是 demote 在 promote 之前。
    let (_dir, pool) = common::make_test_pool().await;
    let first = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    let second = reindex(&pool)
        .run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert_ne!(first.new_rule_version_id, second.new_rule_version_id);

    let (first_status, first_retired): (String, Option<OffsetDateTime>) =
        sqlx::query_as("SELECT status, retired_at FROM rule_versions WHERE id = ?")
            .bind(first.new_rule_version_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(first_status, "superseded", "上一版应被 demote");
    assert!(first_retired.is_some(), "demote 同步写 retired_at");

    let second_status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(second.new_rule_version_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(second_status, "active", "新一版应被 promote");

    // 同 kind 同时刻只有一行 active（partial unique 保证）。
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM rule_versions WHERE kind='reindex' AND status='active'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_count, 1);
}

// --- F15-10 W9-F4: abort 与 dry-run 端到端 -------------------------------

#[tokio::test]
async fn abort_running_job_transitions_to_aborted_and_preserves_data() {
    // running 状态下 abort → aborted；reindex_jobs 行的 last_processed_id /
    // attempt_count / started_at 保留（abort 不回滚已落地的批次），lease
    // 被清空。rule_versions 行**保留 pending**（与 cli-semantics §4.8 line
    // 306 一致：失败/取消不自动清理 pending rule_versions）。
    let (_dir, pool) = common::make_test_pool().await;
    // 先用 start_reindex_tx + claim_by_id 构造 running job
    let repo = rss_ai_news_storage::ReindexJobRepo::new(pool.clone());
    let outcome = repo
        .start_reindex_tx(
            "reindex",
            "tag-abort-1",
            "desc",
            "sha-abort-1",
            "link_hash",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let rule_id = outcome.rule_version_id;
    let job_id = outcome.job_id;
    repo.claim_by_id(
        job_id,
        "worker-x",
        OffsetDateTime::now_utc(),
        OffsetDateTime::now_utc() + Duration::seconds(600),
    )
    .await
    .unwrap()
    .unwrap();

    let abort_outcome = reindex(&pool)
        .abort(job_id, "manual cli abort")
        .await
        .unwrap();
    assert!(abort_outcome.aborted, "running → aborted 必须成功");
    assert_eq!(abort_outcome.target.as_deref(), Some("link_hash"));
    assert_eq!(abort_outcome.previous_state.as_deref(), Some("running"));

    let (state, aborted_reason, lease_owner, finished_at): (
        String,
        Option<String>,
        Option<String>,
        Option<OffsetDateTime>,
    ) = sqlx::query_as(
        "SELECT state, aborted_reason, lease_owner, finished_at FROM reindex_jobs WHERE id = ?",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "aborted");
    assert_eq!(aborted_reason.as_deref(), Some("manual cli abort"));
    assert!(lease_owner.is_none(), "abort 必须清 lease_owner");
    assert!(finished_at.is_some());

    // rule_versions 保持 pending（cli-semantics §4.8 line 306）。
    let rule_status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(rule_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rule_status, "pending");
}

#[tokio::test]
async fn abort_already_terminal_job_is_idempotent_noop() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = rss_ai_news_storage::ReindexJobRepo::new(pool.clone());
    let outcome = repo
        .start_reindex_tx(
            "reindex",
            "tag-abort-2",
            "desc",
            "sha-abort-2",
            "link_hash",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let job_id = outcome.job_id;
    repo.complete_without_claim(job_id, OffsetDateTime::now_utc())
        .await
        .unwrap();

    let abort_outcome = reindex(&pool).abort(job_id, "noop test").await.unwrap();
    assert!(
        !abort_outcome.aborted,
        "terminal 状态 abort 不算成功 → aborted=false"
    );
    assert_eq!(abort_outcome.previous_state.as_deref(), Some("completed"));
}

#[tokio::test]
async fn abort_missing_job_returns_not_found_outcome() {
    let (_dir, pool) = common::make_test_pool().await;
    let abort_outcome = reindex(&pool).abort(99_999, "ghost").await.unwrap();
    assert!(!abort_outcome.aborted);
    assert!(abort_outcome.target.is_none());
    assert!(abort_outcome.previous_state.is_none());
}

#[tokio::test]
async fn dry_run_link_hash_matches_real_run_numbers_and_writes_nothing() {
    // F15-10 dry-run 高保真：scanned/updated/unchanged/errors 与真实 run 一致；
    // 且不写 rule_versions、reindex_jobs，也不改 feed_entries.link_hash。
    let (_dir, pool) = common::make_test_pool().await;
    let source = common::insert_source(
        &pool,
        common::insert_config_rule(&pool).await,
        "dryrun-lh",
        "https://example.com/feed.xml",
    )
    .await;
    let entry_id =
        common::seed_pending_fetch_entry(&pool, source, "dryrun-lh", "wrong", None).await;
    let initial_link_hash: String =
        sqlx::query_scalar("SELECT link_hash FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let dry = reindex(&pool)
        .dry_run(reindex_opts(ReindexTarget::LinkHash, 10))
        .await
        .unwrap();
    assert_eq!(dry.scanned, 1);
    assert_eq!(dry.updated, 1, "wrong → 正确 link_hash 是 would-update");
    assert_eq!(dry.unchanged, 0);
    assert_eq!(dry.new_rule_version_id, 0, "dry-run 不写 rule_versions");
    assert_eq!(dry.reindex_job_id, 0, "dry-run 不写 reindex_jobs");

    let reindex_jobs_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM reindex_jobs WHERE target = 'link_hash'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reindex_jobs_count, 0, "dry-run 不应创建 reindex_jobs 行");

    let post_link_hash: String =
        sqlx::query_scalar("SELECT link_hash FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        post_link_hash, initial_link_hash,
        "dry-run 必须保持 feed_entries 行不变"
    );
}

#[tokio::test]
async fn dry_run_content_hash_distinguishes_unchanged_updated_conflict() {
    // F15-10 dry-run conflict_skipped 必须真实反映 EXISTS 冲突——
    // 用 peek_content_hash_outcome 复用真实 update 的判定逻辑。
    let (_dir, pool) = common::make_test_pool().await;
    let body = "same body";
    let hash = sha256_hex(body.as_bytes());
    // (a) hash == new_hash → unchanged
    common::seed_persisted_article(&pool, &hash, "A", body).await;
    // (b) 另一行 body 相同但 content_hash 错的 → would conflict（同 body 已被 A 占用）
    common::seed_persisted_article(&pool, "wrong-hash", "B", body).await;

    let dry = reindex(&pool)
        .dry_run(reindex_opts(ReindexTarget::ContentHash, 10))
        .await
        .unwrap();
    assert_eq!(dry.scanned, 2);
    assert_eq!(dry.unchanged, 1);
    assert_eq!(dry.conflict_skipped, 1);
    assert_eq!(dry.updated, 0);
}

#[tokio::test]
async fn dry_run_categories_counts_would_archive_without_writing() {
    let (_dir, pool) = common::make_test_pool().await;
    let cfg = common::insert_config_rule(&pool).await;
    common::insert_source(&pool, cfg, "obsolete", "https://example.com/old.xml").await;

    let dry = reindex(&pool)
        .dry_run(reindex_opts(ReindexTarget::Categories, 10))
        .await
        .unwrap();
    assert!(
        dry.archived >= 1,
        "obsolete source 应当被计为 would-archive"
    );
    assert_eq!(dry.reindex_job_id, 0);
    assert_eq!(dry.new_rule_version_id, 0);

    // 确认 feed_sources.status 没有被改成 archived。
    let archived_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feed_sources WHERE status='archived'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(archived_count, 0, "dry-run 不应当真去 archive");
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

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
