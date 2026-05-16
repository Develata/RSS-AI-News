mod common;

use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_feed::{FeedError, FeedFetcher};
use rss_ai_news_runtime::{
    PublishFlow, PublishFreezeOptions, PublishFreezeStatus, PublishInitOptions, PublishInitOutcome,
};
use rss_ai_news_storage::{PublishItemRepo, PublishItemRepository};
use sqlx::SqlitePool;

use common::{
    app_config, full_context, insert_config_rule, make_test_pool, seed_ai_succeeded_article,
    seed_persisted_article_for_passthrough,
};

#[tokio::test]
async fn init_creates_publish_record_returns_created_outcome() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let render = insert_config_rule(&pool).await;
    let policy = insert_config_rule(&pool).await;

    let outcome = flow
        .init(init_opts(render, policy))
        .await
        .expect("init should succeed");

    assert!(matches!(outcome, PublishInitOutcome::Created { .. }));
}

#[tokio::test]
async fn init_returns_already_exists_on_idempotency_key_conflict() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let render = insert_config_rule(&pool).await;
    let policy = insert_config_rule(&pool).await;
    let opts = init_opts(render, policy);
    flow.init(opts.clone()).await.unwrap();

    let outcome = flow.init(opts).await.expect("second init should succeed");

    assert!(matches!(
        outcome,
        PublishInitOutcome::AlreadyExists {
            state,
            ..
        } if state == "pending"
    ));
}

#[tokio::test]
async fn freeze_with_ai_path_inserts_publish_items_and_advances_record_to_snapshot_frozen() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;
    let (article_id, ai_result_id) =
        seed_ai_succeeded_article(&pool, "ai", "runtime-ai", "Title", "body", "summary", 88, 1)
            .await;

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.publish_record_id, publish_record_id);
    assert_eq!(outcome.status, PublishFreezeStatus::Frozen);
    assert_eq!(outcome.item_count, 1);
    assert_record_state(&pool, publish_record_id, "snapshot_frozen").await;
    let items = PublishItemRepo::new(pool.clone())
        .list_by_publish_record(publish_record_id)
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].article_id, article_id);
    assert_eq!(items[0].article_ai_result_id, Some(ai_result_id));
}

#[tokio::test]
async fn freeze_with_ai_off_passthrough_promotes_persisted_articles_in_same_tx() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;
    let article_id =
        seed_persisted_article_for_passthrough(&pool, "ai", "runtime-direct", "Title", "raw").await;

    let outcome = flow.freeze(freeze_opts(false, true)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::Frozen);
    assert_record_state(&pool, publish_record_id, "snapshot_frozen").await;
    assert_article_state(&pool, article_id, "ready_for_publish").await;
}

#[tokio::test]
async fn freeze_returns_snapshot_empty_when_no_candidates_match() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::SnapshotEmpty);
    assert_record_state(&pool, publish_record_id, "failed").await;
    let kind: Option<String> =
        sqlx::query_scalar("SELECT last_error_kind FROM publish_records WHERE id = ?")
            .bind(publish_record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind.as_deref(), Some("snapshot_empty"));
}

#[tokio::test]
async fn freeze_returns_nothing_to_claim_when_no_pending_records() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool);

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::NothingToClaim);
    assert_eq!(outcome.publish_record_id, 0);
}

#[tokio::test]
async fn freeze_skips_articles_without_correct_category_key() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;
    seed_ai_succeeded_article(&pool, "ml", "runtime-ml", "Title", "body", "summary", 88, 1).await;

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::SnapshotEmpty);
    assert_record_state(&pool, publish_record_id, "failed").await;
}

fn flow(pool: SqlitePool) -> PublishFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let ctx = Arc::new(full_context(
        "publish",
        pool,
        app,
        Arc::new(DummyFeedFetcher),
    ));
    PublishFlow::new(ctx)
}

async fn init_record(flow: &PublishFlow, pool: &SqlitePool) -> i64 {
    let render = insert_config_rule(pool).await;
    let policy = insert_config_rule(pool).await;
    match flow.init(init_opts(render, policy)).await.unwrap() {
        PublishInitOutcome::Created { publish_record_id } => publish_record_id,
        PublishInitOutcome::AlreadyExists { .. } => panic!("test key should be unique"),
    }
}

fn init_opts(render_version: i64, selection_policy_version: i64) -> PublishInitOptions {
    PublishInitOptions {
        category_key: "ai".to_string(),
        report_date: "2026-04-28".to_string(),
        target_timezone: "Asia/Shanghai".to_string(),
        render_version,
        selection_policy_version,
        remote_target: None,
    }
}

fn freeze_opts(ai_enabled: bool, include_unscored: bool) -> PublishFreezeOptions {
    PublishFreezeOptions {
        category_key: "ai".to_string(),
        max_items: NonZeroU32::new(10).unwrap(),
        min_importance_score: Score0To100::try_new(50).unwrap(),
        include_unscored,
        ai_enabled,
        excerpt_max_chars: 100,
    }
}

async fn assert_record_state(pool: &SqlitePool, id: i64, expected: &str) {
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, expected);
}

async fn assert_article_state(pool: &SqlitePool, id: i64, expected: &str) {
    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, expected);
}

struct DummyFeedFetcher;

#[async_trait]
impl FeedFetcher for DummyFeedFetcher {
    async fn fetch_raw(
        &self,
        _request: &FeedFetchRequest,
    ) -> Result<rss_ai_news_feed::fetcher::RawFeedFetch, FeedError> {
        Err(FeedError::ConnectionFailed {
            source: "publish tests do not fetch feeds".to_string(),
        })
    }
}
