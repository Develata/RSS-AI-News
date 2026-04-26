mod common;

use rss_ai_news_storage::{NewRunEvent, RunEventRepository, SqliteRunEventRepo};

use common::make_test_pool;

#[tokio::test]
async fn insert_writes_minimal_event() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteRunEventRepo::new(pool.clone());

    let id = repo
        .insert(&event("run-1", None, None))
        .await
        .expect("event should insert");
    let row: (String, Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT event_kind, target_kind, target_id, context_json FROM run_events WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("event should be readable");

    assert_eq!(row, ("run_started".to_string(), None, None, None));
}

#[tokio::test]
async fn insert_writes_full_event_with_target_and_context() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteRunEventRepo::new(pool.clone());

    let id = repo
        .insert(&event(
            "run-1",
            Some(("feed_source".to_string(), 42)),
            Some(r#"{"code":503}"#.to_string()),
        ))
        .await
        .expect("event should insert");
    let row: (String, Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT severity, target_kind, target_id, context_json FROM run_events WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("event should be readable");

    assert_eq!(
        row,
        (
            "warn".to_string(),
            Some("feed_source".to_string()),
            Some(42),
            Some(r#"{"code":503}"#.to_string())
        )
    );
}

#[tokio::test]
async fn insert_two_events_for_same_run_id_returns_distinct_ids() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteRunEventRepo::new(pool);

    let first = repo
        .insert(&event("run-1", None, None))
        .await
        .expect("first event should insert");
    let second = repo
        .insert(&event("run-1", None, None))
        .await
        .expect("second event should insert");

    assert_ne!(first, second);
}

fn event(run_id: &str, target: Option<(String, i64)>, context_json: Option<String>) -> NewRunEvent {
    NewRunEvent {
        run_id: run_id.to_string(),
        trace_id: None,
        stage: "ingest".to_string(),
        severity: "warn".to_string(),
        event_kind: "run_started".to_string(),
        target_kind: target.as_ref().map(|(kind, _)| kind.clone()),
        target_id: target.map(|(_, id)| id),
        message: "message".to_string(),
        context_json,
    }
}
