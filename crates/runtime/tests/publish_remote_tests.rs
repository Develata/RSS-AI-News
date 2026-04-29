mod common;

use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_feed::{FeedError, FeedFetcher};
use rss_ai_news_publish::{LocalFsTarget, PublishError, PublishTarget, PublishedArtifact};
use rss_ai_news_runtime::{PublishFlow, PublishRemoteOptions, PublishRemoteStatus};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{
    app_config, full_context_with_publish_targets, make_test_pool, seed_rendered_publish_record,
};

#[tokio::test]
async fn publish_remote_succeeds_promotes_articles() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), Some(Arc::new(MockSuccessTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert_eq!(outcome.publish_record_id, record_id);
    assert_eq!(outcome.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(outcome.commit_sha.as_deref(), Some("remote-commit-sha"));
    assert_eq!(
        outcome.remote_target.as_deref(),
        Some("github://owner/repo/main/reports/ai.md")
    );
    assert_eq!(outcome.item_count, 1);
    assert_record_state(&pool, record_id, "published_remote").await;
    assert_record_remote_fields(&pool, record_id).await;
    assert_referenced_articles_state(&pool, record_id, "published").await;
}

#[tokio::test]
async fn publish_remote_rate_limit_keeps_state_and_articles() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), Some(Arc::new(MockRateLimitTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert!(matches!(
        outcome.status,
        PublishRemoteStatus::Failed { error_kind } if error_kind == "github_rate_limit"
    ));
    assert_record_state(&pool, record_id, "stored_local").await;
    assert_last_error_kind(&pool, record_id, "github_rate_limit").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
}

#[tokio::test]
async fn publish_remote_auth_failed_is_terminal_without_promoting_articles() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), Some(Arc::new(MockAuthFailTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert!(matches!(
        outcome.status,
        PublishRemoteStatus::Failed { error_kind } if error_kind == "github_auth_failed"
    ));
    assert_record_state(&pool, record_id, "failed").await;
    assert_last_error_kind(&pool, record_id, "github_auth_failed").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
}

#[tokio::test]
async fn publish_remote_missing_target_has_no_side_effects() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), None);

    let outcome = flow.publish_remote(remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::MissingTarget);
    assert_eq!(outcome.publish_record_id, 0);
    assert_record_state(&pool, record_id, "stored_local").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
    let attempt_count: i64 =
        sqlx::query_scalar("SELECT attempt_count FROM publish_records WHERE id = ?")
            .bind(record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempt_count, 0);
}

#[tokio::test]
async fn publish_remote_returns_nothing_to_claim_when_no_stored_local_record() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool, Some(Arc::new(MockSuccessTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::NothingToClaim);
    assert_eq!(outcome.publish_record_id, 0);
}

#[tokio::test]
async fn publish_remote_returns_nothing_to_claim_when_record_is_leased_by_other_owner() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    sqlx::query(
        "UPDATE publish_records SET lease_owner = 'other-owner', lease_expires_at = ? WHERE id = ?",
    )
    .bind(OffsetDateTime::now_utc() + time::Duration::seconds(300))
    .bind(record_id)
    .execute(&pool)
    .await
    .unwrap();
    let flow = flow(pool.clone(), Some(Arc::new(MockSuccessTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::NothingToClaim);
    assert_record_state(&pool, record_id, "stored_local").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
}

async fn seed_stored_local_publish_record(pool: &SqlitePool) -> i64 {
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(
        pool,
        Some("github://owner/repo/main/reports/ai.md".to_string()),
        rendered_at,
    )
    .await;
    sqlx::query(
        r#"
        UPDATE publish_records
        SET state = 'stored_local',
            local_stored_at = ?,
            local_path = 'local/report.md',
            lease_owner = NULL,
            lease_expires_at = NULL
        WHERE id = ?
        "#,
    )
    .bind(rendered_at)
    .bind(record_id)
    .execute(pool)
    .await
    .unwrap();
    record_id
}

fn flow(pool: SqlitePool, remote_target: Option<Arc<dyn PublishTarget>>) -> PublishFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let output_dir = tempfile::tempdir().unwrap();
    let ctx = Arc::new(full_context_with_publish_targets(
        "publish",
        pool,
        app,
        Arc::new(DummyFeedFetcher),
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
        remote_target,
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

async fn assert_record_remote_fields(pool: &SqlitePool, id: i64) {
    let row: (Option<String>, Option<String>, Option<OffsetDateTime>, Option<String>) =
        sqlx::query_as(
            "SELECT commit_sha, remote_target, remote_published_at, local_path FROM publish_records WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(row.0.as_deref(), Some("remote-commit-sha"));
    assert_eq!(
        row.1.as_deref(),
        Some("github://owner/repo/main/reports/ai.md")
    );
    assert!(row.2.is_some());
    assert_eq!(row.3.as_deref(), Some("local/report.md"));
}

async fn assert_last_error_kind(pool: &SqlitePool, id: i64, expected: &str) {
    let kind: Option<String> =
        sqlx::query_scalar("SELECT last_error_kind FROM publish_records WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(kind.as_deref(), Some(expected));
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

fn remote_opts() -> PublishRemoteOptions {
    PublishRemoteOptions {
        category_display_name: "AI".to_string(),
        report_title: "Daily AI".to_string(),
        generated_at: fixed_time(),
    }
}

fn fixed_time() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(0).unwrap()
}

struct MockSuccessTarget;

#[async_trait]
impl PublishTarget for MockSuccessTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        Ok(PublishedArtifact {
            local_path: None,
            commit_sha: Some("remote-commit-sha".to_string()),
            remote_target: Some("github://owner/repo/main/reports/ai.md".to_string()),
        })
    }
}

struct MockRateLimitTarget;

#[async_trait]
impl PublishTarget for MockRateLimitTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        Err(PublishError::GitHubRateLimit {
            reset_at: OffsetDateTime::from_unix_timestamp(1_800).unwrap(),
        })
    }
}

struct MockAuthFailTarget;

#[async_trait]
impl PublishTarget for MockAuthFailTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        Err(PublishError::GitHubAuthFailed("bad token".to_string()))
    }
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
