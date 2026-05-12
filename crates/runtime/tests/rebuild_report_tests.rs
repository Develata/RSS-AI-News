mod common;

use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_feed::{FeedError, FeedFetcher};
use rss_ai_news_publish::LocalFsTarget;
use rss_ai_news_runtime::{
    PublishFlow, PublishStoreLocalOptions, RebuildReportFlow, RebuildReportOptions, RuntimeError,
};
use time::OffsetDateTime;

use common::{
    app_config, full_context_with_publish_target, make_test_pool, seed_rendered_publish_record,
};

#[tokio::test]
async fn rebuild_returns_byte_equal_markdown_to_original_render() {
    let (_dir, pool) = make_test_pool().await;
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(&pool, None, rendered_at).await;
    let output_dir = tempfile::tempdir().unwrap();
    let ctx = Arc::new(full_context_with_publish_target(
        "publish",
        pool.clone(),
        Arc::new(app_config(RetentionPolicy::Always, 1)),
        Arc::new(DummyFeedFetcher),
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
    ));
    let stored = PublishFlow::new(ctx.clone())
        .store_local(PublishStoreLocalOptions {
            category_display_name: "AI".to_string(),
            report_title: "Daily AI".to_string(),
            generated_at: rendered_at,
        })
        .await;
    let path = stored.local_path.expect("local path should be set");
    let original = tokio::fs::read_to_string(path).await.unwrap();

    let rebuilt = RebuildReportFlow::new(ctx)
        .rebuild(RebuildReportOptions {
            publish_record_id: record_id,
            category_display_name: "AI".to_string(),
            report_title: "Daily AI".to_string(),
            generated_at_override: Some(rendered_at),
        })
        .await
        .unwrap();

    assert_eq!(rebuilt.markdown_content, original);
}

#[tokio::test]
async fn rebuild_without_generated_at_override_falls_back_to_record_rendered_at_and_matches_original()
 {
    let (_dir, pool) = make_test_pool().await;
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(&pool, None, rendered_at).await;
    let output_dir = tempfile::tempdir().unwrap();
    let ctx = Arc::new(full_context_with_publish_target(
        "publish",
        pool.clone(),
        Arc::new(app_config(RetentionPolicy::Always, 1)),
        Arc::new(DummyFeedFetcher),
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
    ));
    let stored = PublishFlow::new(ctx.clone())
        .store_local(PublishStoreLocalOptions {
            category_display_name: "AI".to_string(),
            report_title: "Daily AI".to_string(),
            generated_at: rendered_at,
        })
        .await;
    let path = stored.local_path.expect("local path should be set");
    let original = tokio::fs::read_to_string(path).await.unwrap();

    // Critical: not passing generated_at_override. Rebuild must fall back to
    // publish_records.rendered_at (always Some after ADVANCE_RENDERED_SQL
    // ran), NOT to OffsetDateTime::now_utc(), otherwise frontmatter
    // `generated_at` drifts and breaks the byte-equal contract.
    let rebuilt = RebuildReportFlow::new(ctx)
        .rebuild(RebuildReportOptions {
            publish_record_id: record_id,
            category_display_name: "AI".to_string(),
            report_title: "Daily AI".to_string(),
            generated_at_override: None,
        })
        .await
        .unwrap();

    assert_eq!(rebuilt.markdown_content, original);
}

#[tokio::test]
async fn rebuild_returns_error_when_publish_record_id_not_found() {
    let (_dir, pool) = make_test_pool().await;
    let output_dir = tempfile::tempdir().unwrap();
    let ctx = Arc::new(full_context_with_publish_target(
        "rebuild_report",
        pool,
        Arc::new(app_config(RetentionPolicy::Always, 1)),
        Arc::new(DummyFeedFetcher),
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
    ));

    let error = RebuildReportFlow::new(ctx)
        .rebuild(RebuildReportOptions {
            publish_record_id: 999,
            category_display_name: "AI".to_string(),
            report_title: "Daily AI".to_string(),
            generated_at_override: Some(fixed_time()),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, RuntimeError::Config(_)));
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
