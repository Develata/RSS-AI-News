//! B1 (ADR 0012): the publish retry budget is per stage. Each claim spends one
//! attempt of the current stage; every successful stage advance resets the
//! stage counter, so a small `publish_max_attempts` still lets a clean run go
//! freeze → render → store_local → remote. `attempt_count` stays cumulative.

mod common;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_publish::{LocalFsTarget, PublishError, PublishTarget, PublishedArtifact};
use rss_ai_news_runtime::{
    PublishFlow, PublishFreezeOptions, PublishFreezeStatus, PublishInitOptions, PublishInitOutcome,
    PublishRemoteOptions, PublishRemoteStatus, PublishRenderOptions, PublishRenderStatus,
    PublishStoreLocalOptions, PublishStoreLocalStatus,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{
    app_config, insert_config_rule, make_test_pool, publish_deps, seed_ai_succeeded_article,
};

/// Fails the first `failures` remote publishes with a retryable rate limit.
struct FlakyRemote {
    failures: AtomicUsize,
}

#[async_trait]
impl PublishTarget for FlakyRemote {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(PublishError::rate_limit(OffsetDateTime::now_utc()));
        }
        Ok(PublishedArtifact {
            local_path: None,
            commit_sha: Some("sha".to_string()),
            remote_target: Some(format!("github://owner/repo/main/{}", report.relative_path)),
        })
    }
}

fn flow(
    pool: SqlitePool,
    max_attempts: u32,
    remote_failures: usize,
) -> (PublishFlow, tempfile::TempDir) {
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.retry.publish_max_attempts = max_attempts;
    let output = tempfile::tempdir().unwrap();
    let deps = publish_deps(
        pool,
        Arc::new(app),
        Arc::new(LocalFsTarget::new(output.path().to_path_buf())),
        Some(Arc::new(FlakyRemote {
            failures: AtomicUsize::new(remote_failures),
        })),
    );
    (PublishFlow::new(Arc::new(deps)), output)
}

async fn init_remote_record(flow: &PublishFlow, pool: &SqlitePool) -> i64 {
    let render = insert_config_rule(pool).await;
    let policy = insert_config_rule(pool).await;
    match flow
        .init(PublishInitOptions {
            category_key: "ai".to_string(),
            report_date: "2026-04-28".to_string(),
            target_timezone: "Asia/Shanghai".to_string(),
            render_version: render,
            selection_policy_version: policy,
            remote_target: Some("owner/repo:main".to_string()),
        })
        .await
        .unwrap()
    {
        PublishInitOutcome::Created { publish_record_id } => publish_record_id,
        PublishInitOutcome::AlreadyExists { .. } => panic!("fresh key"),
    }
}

fn freeze_opts() -> PublishFreezeOptions {
    PublishFreezeOptions {
        category_key: "ai".to_string(),
        max_items: NonZeroU32::new(10).unwrap(),
        min_importance_score: Score0To100::try_new(50).unwrap(),
        include_unscored: false,
        ai_enabled: true,
        candidate_window_hours: 48,
        excerpt_max_chars: 100,
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

async fn counters(pool: &SqlitePool, id: i64) -> (String, i64, i64) {
    sqlx::query_as(
        "SELECT state, attempt_count, stage_attempt_count FROM publish_records WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn run_local_stages(flow: &PublishFlow, id: i64) {
    assert_eq!(
        flow.freeze_record(id, freeze_opts()).await.status,
        PublishFreezeStatus::Frozen
    );
    assert_eq!(
        flow.render_record(
            id,
            PublishRenderOptions {
                category_display_name: "AI".into(),
                report_title: "Daily AI".into(),
                generated_at: now(),
                path_template: None,
            },
        )
        .await
        .status,
        PublishRenderStatus::Rendered
    );
    assert_eq!(
        flow.store_local_record(
            id,
            PublishStoreLocalOptions {
                category_display_name: "AI".into(),
                report_title: "Daily AI".into(),
                generated_at: now(),
                path_template: None,
            },
        )
        .await
        .status,
        PublishStoreLocalStatus::StoredLocal
    );
}

fn remote_opts() -> PublishRemoteOptions {
    PublishRemoteOptions {
        category_display_name: "AI".into(),
        report_title: "Daily AI".into(),
        generated_at: now(),
        path_template: None,
    }
}

#[tokio::test]
async fn small_budget_completes_all_four_stages_without_failures() {
    let (_dir, pool) = make_test_pool().await;
    let (flow, _output) = flow(pool.clone(), 2, 0);
    let id = init_remote_record(&flow, &pool).await;
    seed_ai_succeeded_article(&pool, "ai", "budget-ok", "Title", "body", "summary", 88, 1).await;

    run_local_stages(&flow, id).await;
    let remote = flow.publish_remote_record(id, remote_opts()).await;

    assert_eq!(remote.status, PublishRemoteStatus::PublishedRemote);
    // Four claims in total (audit), each stage's counter reset on advance.
    assert_eq!(
        counters(&pool, id).await,
        ("published_remote".to_string(), 4, 0)
    );
}

#[tokio::test]
async fn one_retryable_failure_per_stage_is_absorbed_by_that_stage_budget() {
    let (_dir, pool) = make_test_pool().await;
    let (flow, _output) = flow(pool.clone(), 2, 1);
    let id = init_remote_record(&flow, &pool).await;
    seed_ai_succeeded_article(
        &pool,
        "ai",
        "budget-retry",
        "Title",
        "body",
        "summary",
        88,
        1,
    )
    .await;
    run_local_stages(&flow, id).await;

    let first = flow.publish_remote_record(id, remote_opts()).await;
    assert!(matches!(first.status, PublishRemoteStatus::Failed { .. }));
    assert_eq!(
        counters(&pool, id).await,
        ("stored_local".to_string(), 4, 1)
    );
    let second = flow.publish_remote_record(id, remote_opts()).await;

    assert_eq!(second.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(
        counters(&pool, id).await,
        ("published_remote".to_string(), 5, 0)
    );
}

#[tokio::test]
async fn exhausting_one_stage_budget_fails_the_record() {
    let (_dir, pool) = make_test_pool().await;
    let (flow, _output) = flow(pool.clone(), 2, 2);
    let id = init_remote_record(&flow, &pool).await;
    seed_ai_succeeded_article(
        &pool,
        "ai",
        "budget-fail",
        "Title",
        "body",
        "summary",
        88,
        1,
    )
    .await;
    run_local_stages(&flow, id).await;

    flow.publish_remote_record(id, remote_opts()).await;
    let second = flow.publish_remote_record(id, remote_opts()).await;

    assert!(matches!(second.status, PublishRemoteStatus::Failed { .. }));
    assert_eq!(counters(&pool, id).await, ("failed".to_string(), 5, 2));
}
