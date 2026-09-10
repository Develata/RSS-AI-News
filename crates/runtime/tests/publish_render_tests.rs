mod common;

use std::sync::Arc;

use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_runtime::{PublishFlow, PublishRenderOptions, PublishRenderStatus};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{app_config, make_test_pool, publish_deps, seed_snapshot_frozen_publish_record};

#[tokio::test]
async fn render_advances_snapshot_frozen_to_rendered_when_items_exist() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_snapshot_frozen_publish_record(&pool, None).await;
    let flow = flow(pool.clone());

    let outcome = flow.render(render_opts()).await;

    assert_eq!(outcome.publish_record_id, record_id);
    assert_eq!(outcome.status, PublishRenderStatus::Rendered);
    assert_record_state(&pool, record_id, "rendered").await;
    let rendered_at: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT rendered_at FROM publish_records WHERE id = ?")
            .bind(record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(rendered_at.is_some());
}

#[tokio::test]
async fn render_record_claims_requested_snapshot_not_older_one() {
    let (_dir, pool) = make_test_pool().await;
    let stale_id = seed_snapshot_frozen_publish_record(&pool, None).await;
    let record_id = seed_snapshot_frozen_publish_record(&pool, None).await;
    let flow = flow(pool.clone());

    let outcome = flow.render_record(record_id, render_opts()).await;

    assert_eq!(outcome.publish_record_id, record_id);
    assert_eq!(outcome.status, PublishRenderStatus::Rendered);
    assert_record_state(&pool, stale_id, "snapshot_frozen").await;
    assert_record_state(&pool, record_id, "rendered").await;
}

#[tokio::test]
async fn render_returns_nothing_to_claim_when_no_snapshot_frozen_records() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool);

    let outcome = flow.render(render_opts()).await;

    assert_eq!(outcome.publish_record_id, 0);
    assert_eq!(outcome.status, PublishRenderStatus::NothingToClaim);
}

#[tokio::test]
async fn render_returns_failed_when_publish_record_has_no_items() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_snapshot_frozen_publish_record(&pool, None).await;
    sqlx::query("UPDATE publish_items SET position = ? WHERE publish_record_id = ?")
        .bind(i64::MAX)
        .bind(record_id)
        .execute(&pool)
        .await
        .unwrap();
    let flow = flow(pool.clone());

    let outcome = flow.render(render_opts()).await;

    assert_eq!(outcome.publish_record_id, record_id);
    assert!(matches!(
        outcome.status,
        PublishRenderStatus::Failed { error_kind } if error_kind == "render_failed"
    ));
    assert_record_state(&pool, record_id, "failed").await;
}

fn flow(pool: SqlitePool) -> PublishFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let ctx = Arc::new(publish_deps(
        pool,
        app,
        Arc::new(rss_ai_news_publish::LocalFsTarget::new(std::env::temp_dir())),
        None,
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

fn render_opts() -> PublishRenderOptions {
    PublishRenderOptions {
        category_display_name: "AI".to_string(),
        report_title: "Daily AI".to_string(),
        generated_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        path_template: None,
    }
}
