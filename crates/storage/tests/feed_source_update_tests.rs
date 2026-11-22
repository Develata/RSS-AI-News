mod common;

use rss_ai_news_storage::{FeedSourceRepo, FeedSourceRepository};
use time::OffsetDateTime;

/// The endpoint the seeded sources are created with (R2-S12 guard).
const SEEDED: rss_ai_news_storage::FeedEndpoint<'static> = rss_ai_news_storage::FeedEndpoint {
    feed_url: "https://example.com/feed.xml",
    feed_kind: rss_ai_news_domain::state::FeedKind::Rss,
};

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

    repo.update_after_fetch_failure(source_id, SEEDED, now, "boom", "http_5xx")
        .await
        .expect("failure update should succeed");
    let updated = repo
        .update_after_fetch_success(source_id, SEEDED, Some("etag"), Some("last-mod"), now, now)
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

    repo.update_after_fetch_failure(source_id, SEEDED, now, "first", "http_5xx")
        .await
        .expect("first failure update should succeed");
    repo.update_after_fetch_failure(source_id, SEEDED, now, "second", "http_5xx")
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
        .update_after_fetch_success(999, SEEDED, None, None, now, now)
        .await
        .expect("success update should not error");
    let failure = repo
        .update_after_fetch_failure(999, SEEDED, now, "missing", "not_found")
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
    repo.update_after_fetch_success(source_id, SEEDED, Some("\"v1\""), Some("lm-1"), now, now)
        .await
        .unwrap();
    repo.update_after_fetch_failure(source_id, SEEDED, now, "boom", "http_5xx")
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
        repo.update_after_not_modified(source_id, SEEDED, None, None, now, now)
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

    repo.update_after_not_modified(source_id, SEEDED, Some("\"v2\""), None, now, now)
        .await
        .unwrap();
    assert_eq!(validators(pool.clone()).await.0.as_deref(), Some("\"v2\""));

    // 200 without validators: replace semantics (the new representation has none).
    repo.update_after_fetch_success(source_id, SEEDED, None, None, now, now)
        .await
        .unwrap();
    let (etag, last_modified, _, _) = validators(pool.clone()).await;
    assert_eq!((etag, last_modified), (None, None));

    assert!(
        !repo
            .update_after_not_modified(999, SEEDED, None, None, now, now)
            .await
            .unwrap(),
        "missing row reports false"
    );
}

/// R2-S12: a response for the old endpoint arrives after config moved the
/// source (and reset its validators): none of the write-backs lands.
#[tokio::test]
async fn write_backs_for_a_moved_endpoint_match_no_row() {
    let (_dir, pool) = make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let repo = FeedSourceRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();
    // Config sync moved the source to another feed while request A was out.
    sqlx::query("UPDATE feed_sources SET feed_url = 'https://example.com/b.xml' WHERE id = ?")
        .bind(source_id)
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        !repo
            .update_after_not_modified(source_id, SEEDED, Some("\"a-etag\""), None, now, now)
            .await
            .unwrap()
    );
    assert!(
        !repo
            .update_after_fetch_success(source_id, SEEDED, Some("\"a-etag\""), None, now, now)
            .await
            .unwrap()
    );
    assert!(
        !repo
            .update_after_fetch_failure(source_id, SEEDED, now, "boom", "http_5xx")
            .await
            .unwrap()
    );
    let (etag, failures): (Option<String>, i64) =
        sqlx::query_as("SELECT etag, consecutive_failures FROM feed_sources WHERE id = ?")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(etag, None, "B did not get A's ETag");
    assert_eq!(failures, 0);

    // The current endpoint still records normally.
    let moved = rss_ai_news_storage::FeedEndpoint {
        feed_url: "https://example.com/b.xml",
        ..SEEDED
    };
    assert!(
        repo.update_after_not_modified(source_id, moved, Some("\"b\""), None, now, now)
            .await
            .unwrap()
    );
}
