mod common;

use std::sync::Arc;

use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_runtime::{IngestFlow, IngestOptions};
use rss_ai_news_storage::{NewRawArtifact, RawArtifactRepo, RawArtifactRepository};
use time::{Duration, OffsetDateTime};

#[tokio::test]
async fn ingest_start_purges_one_bounded_batch_even_when_retention_is_off() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = RawArtifactRepo::new(pool.clone());
    for id in 0..501 {
        repo.upsert_inline(&NewRawArtifact {
            kind: "feed_payload".into(),
            artifact_key: id.to_string(),
            content_encoding: "utf-8".into(),
            inline_body: b"expired".to_vec(),
            byte_size: 7,
            sha256: "fixture".into(),
            retention_policy: "on_failure".into(),
            expires_at: Some(OffsetDateTime::now_utc() - Duration::days(1)),
        })
        .await
        .unwrap();
    }
    let deps = common::ingest_deps(
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Off, 1)),
        Arc::new(common::DummyFeedFetcher),
    );
    let flow = IngestFlow::new(Arc::new(deps), Vec::new());
    flow.run(IngestOptions::default()).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "one run deletes at most 500 expired artifacts");
    flow.run(IngestOptions::default()).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn ai_start_purges_artifacts_before_claim_and_records_actual_count() {
    use rss_ai_news_domain::Score0To100;
    use rss_ai_news_runtime::{AiRunFlow, AiRunOptions};
    let (_dir, pool) = common::make_test_pool().await;
    let repo = RawArtifactRepo::new(pool.clone());
    let id = repo
        .upsert_inline(&NewRawArtifact {
            kind: "ai_raw_response".into(),
            artifact_key: "expired".into(),
            content_encoding: "utf-8".into(),
            inline_body: b"expired".to_vec(),
            byte_size: 7,
            sha256: "fixture".into(),
            retention_policy: "on_failure".into(),
            expires_at: Some(OffsetDateTime::now_utc() - Duration::days(1)),
        })
        .await
        .unwrap();
    let deps = common::ai_deps(
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Off, 1)),
        Arc::new(common::DummyAiClient),
    );
    let flow = AiRunFlow::new(Arc::new(deps));
    let opts = AiRunOptions {
        task_gen_batch_size: 1,
        process_batch_size: 1,
        max_attempts: 1,
        prompt_template: "{body_text}".into(),
        model_id: "fixture".into(),
        fallback_models: Vec::new(),
        max_input_chars: 100,
        max_tokens: 20,
        temperature: 0.0,
        min_importance_score: Score0To100::try_new(0).unwrap(),
        max_batches: 1,
        category_key: "ai".into(),
        prompt_version: 1,
        output_schema_version: 1,
    };
    assert_eq!(flow.process_ai_tasks(&opts).await.claimed, 0);
    assert!(repo.find_by_id(id).await.unwrap().is_none());
    flow.process_ai_tasks(&opts).await;
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT context_json FROM run_events WHERE event_kind='artifacts_purged'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(events.len(), 1, "zero deletion emits no duplicate event");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&events[0]).unwrap()["count"],
        1
    );
}

#[tokio::test]
async fn cleanup_failure_does_not_abort_ingest() {
    use rss_ai_news_storage::StoragePool;
    let (dir, pool) = common::make_test_pool().await;
    let read_only = StoragePool::build_read_only(
        &format!("sqlite:{}", dir.join("test.sqlite").display()),
        5_000,
    )
    .await
    .unwrap();
    let mut deps = common::ingest_deps(
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Off, 1)),
        Arc::new(common::DummyFeedFetcher),
    );
    deps.artifact_repo = Arc::new(RawArtifactRepo::new_with_storage(read_only));
    let flow = IngestFlow::new(
        Arc::new(deps),
        vec![common::category_with_sources(&["fixture"])],
    );
    let summary = flow.run(IngestOptions::default()).await;
    assert_eq!(summary.sources_attempted, 1);
    assert_eq!(
        summary.sources_failed, 1,
        "the dummy fetcher is still invoked after cleanup fails"
    );
    let completed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM run_events WHERE event_kind='run_completed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(completed, 1);
}
