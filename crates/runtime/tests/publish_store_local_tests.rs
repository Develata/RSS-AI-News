mod common;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_feed::{FeedError, FeedFetcher};
use rss_ai_news_publish::LocalFsTarget;
use rss_ai_news_runtime::{PublishFlow, PublishStoreLocalOptions, PublishStoreLocalStatus};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{
    MockFailingTarget, MockOnceRetryableThenInner, app_config, full_context_with_publish_target,
    make_test_pool, seed_rendered_publish_record,
};

#[tokio::test]
async fn store_local_with_no_remote_target_publishes_locally_and_promotes_articles() {
    let (_dir, pool) = make_test_pool().await;
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(&pool, None, rendered_at).await;
    let output_dir = tempfile::tempdir().unwrap();
    let flow = flow(
        pool.clone(),
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
    );

    let outcome = flow.store_local(store_opts(rendered_at)).await;

    assert_eq!(outcome.publish_record_id, record_id);
    assert_eq!(outcome.status, PublishStoreLocalStatus::PublishedLocal);
    let local_path = outcome.local_path.expect("local path should be set");
    assert!(Path::new(&local_path).exists());
    assert_record_state(&pool, record_id, "published_local").await;
    assert_referenced_articles_state(&pool, record_id, "published").await;
}

#[tokio::test]
async fn store_local_with_remote_target_advances_to_stored_local_without_promoting_articles() {
    let (_dir, pool) = make_test_pool().await;
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(
        &pool,
        Some("github://owner/repo/main/archive/ai.md".to_string()),
        rendered_at,
    )
    .await;
    let output_dir = tempfile::tempdir().unwrap();
    let flow = flow(
        pool.clone(),
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
    );

    let outcome = flow.store_local(store_opts(rendered_at)).await;

    assert_eq!(outcome.status, PublishStoreLocalStatus::StoredLocal);
    assert_record_state(&pool, record_id, "stored_local").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
}

#[tokio::test]
async fn store_local_returns_nothing_to_claim_when_no_rendered_records() {
    let (_dir, pool) = make_test_pool().await;
    let output_dir = tempfile::tempdir().unwrap();
    let flow = flow(
        pool,
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
    );

    let outcome = flow.store_local(store_opts(fixed_time())).await;

    assert_eq!(outcome.publish_record_id, 0);
    assert_eq!(outcome.status, PublishStoreLocalStatus::NothingToClaim);
}

#[tokio::test]
async fn store_local_returns_article_conflict_when_promote_target_already_advanced() {
    let (_dir, pool) = make_test_pool().await;
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(&pool, None, rendered_at).await;
    sqlx::query(
        "UPDATE articles SET state = 'published' WHERE id IN (SELECT article_id FROM publish_items WHERE publish_record_id = ?)",
    )
    .bind(record_id)
    .execute(&pool)
    .await
    .unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let flow = flow(
        pool.clone(),
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
    );

    let outcome = flow.store_local(store_opts(rendered_at)).await;

    assert!(matches!(
        outcome.status,
        PublishStoreLocalStatus::ArticleConflict { .. }
    ));
    assert_record_state(&pool, record_id, "rendered").await;
}

#[tokio::test]
async fn store_local_returns_failed_with_local_io_error_when_target_dir_unwritable() {
    let (_dir, pool) = make_test_pool().await;
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(&pool, None, rendered_at).await;
    let flow = flow(pool.clone(), Arc::new(MockFailingTarget));

    let outcome = flow.store_local(store_opts(rendered_at)).await;

    assert!(matches!(
        outcome.status,
        PublishStoreLocalStatus::Failed { error_kind } if error_kind == "local_io_error"
    ));
    assert_record_state(&pool, record_id, "failed").await;
}

#[tokio::test]
async fn store_local_retryable_failure_keeps_rendered_state_and_reclaim_succeeds() {
    let (_dir, pool) = make_test_pool().await;
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(&pool, None, rendered_at).await;
    let output_dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf()))
        as Arc<dyn rss_ai_news_publish::PublishTarget>;
    let target = Arc::new(MockOnceRetryableThenInner::new(inner));
    let flow = flow(pool.clone(), target);

    let first = flow.store_local(store_opts(rendered_at)).await;
    assert!(
        matches!(
            first.status,
            PublishStoreLocalStatus::Failed { ref error_kind } if error_kind == "local_io_error"
        ),
        "first attempt must surface retryable LocalIoError as Failed; got: {:?}",
        first.status
    );
    assert_record_state(&pool, record_id, "rendered").await;

    let second = flow.store_local(store_opts(rendered_at)).await;
    assert_eq!(second.publish_record_id, record_id);
    assert_eq!(second.status, PublishStoreLocalStatus::PublishedLocal);
    assert!(
        Path::new(&second.local_path.expect("local path should be set")).exists(),
        "second attempt must materialize the local file"
    );
    assert_record_state(&pool, record_id, "published_local").await;
    assert_referenced_articles_state(&pool, record_id, "published").await;
}

fn flow(pool: SqlitePool, target: Arc<dyn rss_ai_news_publish::PublishTarget>) -> PublishFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let ctx = Arc::new(full_context_with_publish_target(
        "publish",
        pool,
        app,
        Arc::new(DummyFeedFetcher),
        target,
    ));
    PublishFlow::new(ctx)
}

async fn assert_record_state(pool: &SqlitePool, id: i64, expected: &str) {
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, expected);
}

async fn assert_referenced_articles_state(pool: &SqlitePool, record_id: i64, expected: &str) {
    let states: Vec<String> = sqlx::query_scalar(
        "SELECT a.state FROM articles a JOIN publish_items pi ON pi.article_id = a.id WHERE pi.publish_record_id = ? ORDER BY a.id",
    )
    .bind(record_id)
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(!states.is_empty());
    assert!(states.iter().all(|state| state == expected));
}

fn store_opts(generated_at: OffsetDateTime) -> PublishStoreLocalOptions {
    PublishStoreLocalOptions {
        category_display_name: "AI".to_string(),
        report_title: "Daily AI".to_string(),
        generated_at,
    }
}

fn fixed_time() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(0).unwrap()
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
