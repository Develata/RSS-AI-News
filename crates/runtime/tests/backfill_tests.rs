//! W9c — backfill flow（extract / ai）端到端单测。
//!
//!   - `--target extract`：把时间窗内 Failed/FallbackPersisted 的 feed_entries
//!     重置回 PendingFetch，不动 Persisted 行。
//!   - `--target ai`：新建 prompt_version 行 + 为候选 articles 批量插入 pending
//!     ai 任务（不改 article.state，ON CONFLICT 去重），分页覆盖全集。

mod common;

use std::sync::Arc;

use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_runtime::{BackfillAiOptions, BackfillExtractOptions, BackfillFlow};
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

fn backfill(pool: &SqlitePool) -> BackfillFlow {
    BackfillFlow::new(Arc::new(common::full_context(
        "backfill",
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Always, 1)),
        Arc::new(common::DummyFeedFetcher),
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
