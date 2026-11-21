mod common;

use rss_ai_news_storage::{FeedSourceRepo, FeedSourceRepository};
use time::OffsetDateTime;

use common::{make_test_pool, seed_source};

type SourceFetchState = (
    Option<String>,
    Option<String>,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    i64,
    Option<String>,
    Option<String>,
);

#[tokio::test]
async fn update_after_fetch_success_clears_error_and_zeroes_failures() {
    let (_dir, pool) = make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let repo = FeedSourceRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();

    repo.update_after_fetch_failure(source_id, now, "boom", "http_5xx")
        .await
        .expect("failure update should succeed");
    let updated = repo
        .update_after_fetch_success(source_id, Some("etag"), Some("last-mod"), now, now)
        .await
        .expect("success update should succeed");
    let row: SourceFetchState = sqlx::query_as(
        r#"
        SELECT etag, last_modified, last_fetched_at, last_success_at,
               consecutive_failures, last_error, last_error_kind
        FROM feed_sources WHERE id = ?
        "#,
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .expect("source should be readable");

    assert!(updated);
    assert_eq!(row.0.as_deref(), Some("etag"));
    assert_eq!(row.1.as_deref(), Some("last-mod"));
    assert!(row.2.is_some());
    assert!(row.3.is_some());
    assert_eq!(row.4, 0);
    assert!(row.5.is_none());
    assert!(row.6.is_none());
}

#[tokio::test]
async fn update_after_fetch_failure_increments_failures() {
    let (_dir, pool) = make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let repo = FeedSourceRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();

    repo.update_after_fetch_failure(source_id, now, "first", "http_5xx")
        .await
        .expect("first failure update should succeed");
    repo.update_after_fetch_failure(source_id, now, "second", "http_5xx")
        .await
        .expect("second failure update should succeed");
    let row: (i64, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT consecutive_failures, last_error, last_error_kind FROM feed_sources WHERE id = ?",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .expect("source should be readable");

    assert_eq!(row.0, 2);
    assert_eq!(row.1.as_deref(), Some("second"));
    assert_eq!(row.2.as_deref(), Some("http_5xx"));
}

#[tokio::test]
async fn update_after_unknown_id_returns_false() {
    let (_dir, pool) = make_test_pool().await;
    let repo = FeedSourceRepo::new(pool);
    let now = OffsetDateTime::now_utc();

    let success = repo
        .update_after_fetch_success(999, None, None, now, now)
        .await
        .expect("success update should not error");
    let failure = repo
        .update_after_fetch_failure(999, now, "missing", "not_found")
        .await
        .expect("failure update should not error");

    assert!(!success);
    assert!(!failure);
}

/// A7: a 304 that omits the validators keeps the stored ones (acceptance
/// 01-feed-ingest: "304 → last_etag / last_modified 不变"); validators the 304
/// does send replace the stored ones. The 200 path keeps replace semantics.
#[tokio::test]
async fn not_modified_update_keeps_validators_the_304_omits() {
    let (_dir, pool) = make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let repo = FeedSourceRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();
    repo.update_after_fetch_success(source_id, Some("\"v1\""), Some("lm-1"), now, now)
        .await
        .unwrap();
    repo.update_after_fetch_failure(source_id, now, "boom", "http_5xx")
        .await
        .unwrap();

    let validators = |pool: sqlx::SqlitePool| async move {
        sqlx::query_as::<_, (Option<String>, Option<String>, i64, Option<String>)>(
            "SELECT etag, last_modified, consecutive_failures, last_error_kind \
             FROM feed_sources WHERE id = ?",
        )
        .bind(source_id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    assert!(
        repo.update_after_not_modified(source_id, None, None, now, now)
            .await
            .unwrap()
    );
    assert_eq!(
        validators(pool.clone()).await,
        (
            Some("\"v1\"".to_string()),
            Some("lm-1".to_string()),
            0,
            None
        )
    );

    repo.update_after_not_modified(source_id, Some("\"v2\""), None, now, now)
        .await
        .unwrap();
    assert_eq!(validators(pool.clone()).await.0.as_deref(), Some("\"v2\""));

    // 200 without validators: replace semantics (the new representation has none).
    repo.update_after_fetch_success(source_id, None, None, now, now)
        .await
        .unwrap();
    let (etag, last_modified, _, _) = validators(pool.clone()).await;
    assert_eq!((etag, last_modified), (None, None));

    assert!(
        !repo
            .update_after_not_modified(999, None, None, now, now)
            .await
            .unwrap(),
        "missing row reports false"
    );
}
