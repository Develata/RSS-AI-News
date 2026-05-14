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
