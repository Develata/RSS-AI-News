mod common;

use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_feed::{FeedError, FeedFetcher, fetcher::RawFeedFetch};
use rss_ai_news_runtime::{
    BackfillAiOptions, BackfillExtractOptions, BackfillFlow, ReindexFlow, ReindexOptions,
    ReindexTarget,
};
use rss_ai_news_storage::ReindexJobRepository;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

#[tokio::test]
async fn backfill_extract_resets_failed_entries_in_window() {
    let (_dir, pool) = common::make_test_pool().await;
    seed_failed_entry(&pool, "failed", OffsetDateTime::now_utc()).await;
    let summary = backfill(&pool)
        .extract(BackfillExtractOptions {
            date_from: None,
            date_to: None,
        })
        .await
        .unwrap();
    assert_eq!(summary.reset, 1);
}

#[tokio::test]
async fn backfill_extract_does_not_touch_persisted_entries() {
    let (_dir, pool) = common::make_test_pool().await;
    seed_failed_entry(&pool, "persisted", OffsetDateTime::now_utc()).await;
    let summary = backfill(&pool)
        .extract(BackfillExtractOptions {
            date_from: None,
            date_to: None,
        })
        .await
        .unwrap();
    assert_eq!(summary.examined, 1);
    assert_eq!(summary.reset, 0);
}

#[tokio::test]
async fn backfill_extract_with_no_window_resets_all_failed() {
    let (_dir, pool) = common::make_test_pool().await;
    seed_failed_entry(&pool, "failed", OffsetDateTime::now_utc()).await;
    seed_failed_entry(&pool, "failed", OffsetDateTime::now_utc()).await;
    let summary = backfill(&pool)
        .extract(BackfillExtractOptions {
            date_from: None,
            date_to: None,
        })
        .await
        .unwrap();
    assert_eq!(summary.reset, 2);
}

#[tokio::test]
async fn backfill_extract_examined_counts_window_intersection() {
    let (_dir, pool) = common::make_test_pool().await;
    let now = OffsetDateTime::now_utc();
    seed_failed_entry(&pool, "failed", now - Duration::days(3)).await;
    seed_failed_entry(&pool, "failed", now).await;
    let summary = backfill(&pool)
        .extract(BackfillExtractOptions {
            date_from: Some(now - Duration::days(1)),
            date_to: None,
        })
        .await
        .unwrap();
    assert_eq!(summary.examined, 1);
}

#[tokio::test]
async fn backfill_ai_creates_new_prompt_version_row() {
    let (_dir, pool) = common::make_test_pool().await;
    common::seed_persisted_article(&pool, "bf-a", "A", "body text").await;
    let summary = backfill(&pool)
        .ai(ai_opts(&pool, "prompt-a", 10).await)
        .await
        .unwrap();
    assert!(summary.new_prompt_version_id > 0);
}

#[tokio::test]
async fn backfill_ai_inserts_pending_for_persisted_without_state_change() {
    let (_dir, pool) = common::make_test_pool().await;
    let article = common::seed_persisted_article(&pool, "bf-b", "B", "body text").await;
    let summary = backfill(&pool)
        .ai(ai_opts(&pool, "prompt-b", 10).await)
        .await
        .unwrap();
    assert_eq!(summary.ai_tasks_inserted, 1);
    assert_eq!(article_state(&pool, article).await, "persisted");
}

#[tokio::test]
async fn backfill_ai_inserts_pending_for_ai_done_without_state_change() {
    let (_dir, pool) = common::make_test_pool().await;
    let article = common::seed_persisted_article(&pool, "bf-c", "C", "body text").await;
    sqlx::query("UPDATE articles SET state = 'ai_done' WHERE id = ?")
        .bind(article)
        .execute(&pool)
        .await
        .unwrap();
    let summary = backfill(&pool)
        .ai(ai_opts(&pool, "prompt-c", 10).await)
        .await
        .unwrap();
    assert_eq!(summary.ai_tasks_inserted, 1);
    assert_eq!(article_state(&pool, article).await, "ai_done");
}

#[tokio::test]
async fn backfill_ai_skips_already_existing_tuple() {
    let (_dir, pool) = common::make_test_pool().await;
    let article = common::seed_persisted_article(&pool, "bf-d", "D", "body text").await;
    let opts = ai_opts(&pool, "prompt-d", 10).await;
    let schema = opts.output_schema_version;
    let first = backfill(&pool).ai(opts).await.unwrap();
    sqlx::query(
        "INSERT INTO article_ai_results (article_id, prompt_version, output_schema_version, model_id, state) VALUES (?, ?, ?, 'test-model', 'pending') ON CONFLICT DO NOTHING",
    )
    .bind(article)
    .bind(first.new_prompt_version_id)
    .bind(schema)
    .execute(&pool)
    .await
    .unwrap();
    let mut second_opts = ai_opts(&pool, "prompt-d", 10).await;
    second_opts.output_schema_version = schema;
    let second = backfill(&pool).ai(second_opts).await.unwrap();
    assert_eq!(second.ai_tasks_conflict, 1);
}

#[tokio::test]
async fn backfill_ai_pagination_covers_all_articles() {
    let (_dir, pool) = common::make_test_pool().await;
    common::seed_persisted_article(&pool, "bf-e1", "E1", "body text").await;
    common::seed_persisted_article(&pool, "bf-e2", "E2", "body text").await;
    let summary = backfill(&pool)
        .ai(ai_opts(&pool, "prompt-e", 1).await)
        .await
        .unwrap();
    assert_eq!(summary.articles_scanned, 2);
}

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
    let repo = rss_ai_news_storage::SqliteReindexJobRepo::new(pool.clone());
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
    let repo = rss_ai_news_storage::SqliteReindexJobRepo::new(pool.clone());
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

// --- F15-12 W9-F4: 端到端 reindex 8 用例集 -------------------------------
//
// 覆盖 reindex 流水线在 F15-7..F15-11 全部 lease/跨表 TX 原语接入后的端到端
// 行为合同：幂等 / 批次边界 / crash recovery / 隔离 / 并发拒绝 / failure 不
// 污染 active / 部分失败 continue / dry-run 不污染 rule_versions 链。

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
    let repo = rss_ai_news_storage::SqliteReindexJobRepo::new(pool.clone());

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
    let repo = rss_ai_news_storage::SqliteReindexJobRepo::new(pool.clone());

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

    let repo = rss_ai_news_storage::SqliteReindexJobRepo::new(pool.clone());
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

fn backfill(pool: &SqlitePool) -> BackfillFlow {
    BackfillFlow::new(Arc::new(common::full_context(
        "backfill",
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Always, 1)),
        Arc::new(DummyFeedFetcher),
    )))
}

fn reindex(pool: &SqlitePool) -> ReindexFlow {
    ReindexFlow::new(Arc::new(common::full_context(
        "reindex",
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Always, 1)),
        Arc::new(DummyFeedFetcher),
    )))
}

async fn ai_opts(pool: &SqlitePool, tag: &str, batch_size: u32) -> BackfillAiOptions {
    let output_schema_version = common::seed_output_schema_rule_version(pool).await;
    BackfillAiOptions {
        date_from: None,
        date_to: None,
        batch_size,
        new_prompt_version_tag: tag.to_string(),
        new_prompt_version_sha256: format!("sha-{tag}"),
        new_prompt_version_description: "test".to_string(),
        model_id: "test-model".to_string(),
        output_schema_version,
    }
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

async fn seed_failed_entry(pool: &SqlitePool, state: &str, created_at: OffsetDateTime) {
    let cfg = common::insert_config_rule(pool).await;
    let source = common::insert_source(
        pool,
        cfg,
        &format!("src-{state}-{created_at}"),
        "https://example.com/feed.xml",
    )
    .await;
    let id = common::seed_pending_fetch_entry(
        pool,
        source,
        &format!("uid-{state}-{created_at}"),
        "link",
        None,
    )
    .await;
    sqlx::query("UPDATE feed_entries SET state = ?, created_at = ? WHERE id = ?")
        .bind(state)
        .bind(created_at)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn article_state(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM articles WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct DummyFeedFetcher;

#[async_trait]
impl FeedFetcher for DummyFeedFetcher {
    async fn fetch_raw(&self, _request: &FeedFetchRequest) -> Result<RawFeedFetch, FeedError> {
        Err(FeedError::ConnectionFailed {
            source: "dummy".to_string(),
        })
    }
}
