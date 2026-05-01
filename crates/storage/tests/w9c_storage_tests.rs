mod common;

use rss_ai_news_domain::{
    model::FeedSource,
    state::{FeedKind, FeedSourceStatus},
};
use rss_ai_news_storage::{
    ArticleRepository, FeedEntryRepository, FeedSourceRepository, NewRawArtifact,
    RawArtifactRepository, ResetFailedFilter, SqliteArticleRepo, SqliteFeedEntryRepo,
    SqliteFeedSourceRepo, SqliteRawArtifactRepo, UpdateContentHashOutcome,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

#[tokio::test]
async fn raw_artifact_find_by_id_found() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = SqliteRawArtifactRepo::new(pool);
    let id = repo
        .upsert_inline(&artifact("feed_payload", "k1"))
        .await
        .unwrap();

    let found = repo.find_by_id(id).await.unwrap().unwrap();

    assert_eq!(found.artifact_key, "k1");
}

#[tokio::test]
async fn raw_artifact_find_by_id_missing() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = SqliteRawArtifactRepo::new(pool);

    assert!(repo.find_by_id(99).await.unwrap().is_none());
}

#[tokio::test]
async fn reset_failed_resets_all_failed() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = common::seed_source(&pool).await;
    insert_entry_state(&pool, source_id, "a", "failed", OffsetDateTime::now_utc()).await;
    insert_entry_state(&pool, source_id, "b", "failed", OffsetDateTime::now_utc()).await;
    let repo = SqliteFeedEntryRepo::new(pool.clone());

    let outcome = repo
        .reset_failed_in_window(&ResetFailedFilter::default())
        .await
        .unwrap();

    assert_eq!(outcome.reset, 2);
    assert_eq!(count_state(&pool, "discovered").await, 2);
}

#[tokio::test]
async fn reset_failed_honors_window() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = common::seed_source(&pool).await;
    let now = OffsetDateTime::now_utc();
    insert_entry_state(&pool, source_id, "old", "failed", now - Duration::days(3)).await;
    insert_entry_state(&pool, source_id, "new", "failed", now).await;
    let repo = SqliteFeedEntryRepo::new(pool.clone());

    let outcome = repo
        .reset_failed_in_window(&ResetFailedFilter {
            date_from: Some(now - Duration::days(1)),
            date_to: None,
        })
        .await
        .unwrap();

    assert_eq!(outcome.examined, 1);
    assert_eq!(outcome.reset, 1);
}

#[tokio::test]
async fn reset_failed_ignores_non_failed() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = common::seed_source(&pool).await;
    insert_entry_state(
        &pool,
        source_id,
        "ok",
        "persisted",
        OffsetDateTime::now_utc(),
    )
    .await;
    let repo = SqliteFeedEntryRepo::new(pool);

    let outcome = repo
        .reset_failed_in_window(&ResetFailedFilter::default())
        .await
        .unwrap();

    assert_eq!(outcome.examined, 1);
    assert_eq!(outcome.reset, 0);
}

#[tokio::test]
async fn article_backfill_lists_all_non_retired() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_rule, _entry, article_id) = common::seed_article(&pool).await;
    let repo = SqliteArticleRepo::new(pool);

    let rows = repo
        .list_in_window_for_backfill(None, None, 10, 0)
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].article_id, article_id);
}

#[tokio::test]
async fn article_backfill_honors_date_from() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_rule, _entry, article_id) = common::seed_article(&pool).await;
    let now = OffsetDateTime::now_utc();
    sqlx::query("UPDATE articles SET created_at = ? WHERE id = ?")
        .bind(now)
        .bind(article_id)
        .execute(&pool)
        .await
        .unwrap();
    let repo = SqliteArticleRepo::new(pool);

    let rows = repo
        .list_in_window_for_backfill(Some(now - Duration::hours(1)), None, 10, 0)
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn article_backfill_honors_date_to_and_after_id() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_rule, _entry, first) = common::seed_article(&pool).await;
    let second = insert_article_with_hash(&pool, "content-hash-second").await;
    let repo = SqliteArticleRepo::new(pool);

    let rows = repo
        .list_in_window_for_backfill(
            None,
            Some(OffsetDateTime::now_utc() + Duration::days(1)),
            10,
            first,
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].article_id, second);
}

#[tokio::test]
async fn link_hash_reindex_lists_after_id() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = common::seed_source(&pool).await;
    let first = common::insert_feed_entry(&pool, source_id, "a", "old-a").await;
    let second = common::insert_feed_entry(&pool, source_id, "b", "old-b").await;
    let repo = SqliteFeedEntryRepo::new(pool);

    let rows = repo.list_for_link_hash_reindex(first, 10).await.unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, second);
}

#[tokio::test]
async fn update_link_hash_success() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = common::seed_source(&pool).await;
    let id = common::insert_feed_entry(&pool, source_id, "a", "old").await;
    let repo = SqliteFeedEntryRepo::new(pool.clone());

    assert!(repo.update_link_hash(id, "new").await.unwrap());
    assert_eq!(entry_link_hash(&pool, id).await, "new");
}

#[tokio::test]
async fn update_link_hash_missing_false() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = SqliteFeedEntryRepo::new(pool);

    assert!(!repo.update_link_hash(99, "new").await.unwrap());
}

#[tokio::test]
async fn update_content_hash_updated() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_rule, _entry, id) = common::seed_article(&pool).await;
    let repo = SqliteArticleRepo::new(pool);

    assert_eq!(
        repo.update_content_hash(id, "new-content-hash")
            .await
            .unwrap(),
        UpdateContentHashOutcome::Updated
    );
}

#[tokio::test]
async fn update_content_hash_conflict() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_rule, _entry, id) = common::seed_article(&pool).await;
    let other = insert_article_with_hash(&pool, "other-hash").await;
    let other_hash = article_hash(&pool, other).await;
    let repo = SqliteArticleRepo::new(pool);

    assert_eq!(
        repo.update_content_hash(id, &other_hash).await.unwrap(),
        UpdateContentHashOutcome::Conflict
    );
}

#[tokio::test]
async fn update_content_hash_unchanged() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_rule, _entry, id) = common::seed_article(&pool).await;
    let hash = article_hash(&pool, id).await;
    let repo = SqliteArticleRepo::new(pool);

    assert_eq!(
        repo.update_content_hash(id, &hash).await.unwrap(),
        UpdateContentHashOutcome::Unchanged
    );
}

#[tokio::test]
async fn feed_source_list_all_returns_all_statuses() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = SqliteFeedSourceRepo::new(pool.clone());
    let config_version = common::insert_rule(&pool, "config", "cfg-list-all", "sha-list-all").await;
    repo.upsert(&feed_source(
        "ai",
        "active",
        FeedSourceStatus::Active,
        config_version,
    ))
    .await
    .unwrap();
    repo.upsert(&feed_source(
        "ai",
        "paused",
        FeedSourceStatus::Paused,
        config_version,
    ))
    .await
    .unwrap();

    let rows = repo.list_all().await.unwrap();

    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn feed_source_mark_archived_once() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = SqliteFeedSourceRepo::new(pool.clone());
    let config_version = common::insert_rule(&pool, "config", "cfg-archive", "sha-archive").await;
    let id = repo
        .upsert(&feed_source(
            "ai",
            "active",
            FeedSourceStatus::Active,
            config_version,
        ))
        .await
        .unwrap();

    assert!(repo.mark_archived(id).await.unwrap());
    assert!(!repo.mark_archived(id).await.unwrap());
}

#[tokio::test]
async fn feed_source_archived_hidden_from_list_by_category() {
    let (_dir, pool) = common::make_test_pool().await;
    let repo = SqliteFeedSourceRepo::new(pool.clone());
    let config_version = common::insert_rule(&pool, "config", "cfg-hidden", "sha-hidden").await;
    let id = repo
        .upsert(&feed_source(
            "ai",
            "active",
            FeedSourceStatus::Active,
            config_version,
        ))
        .await
        .unwrap();
    repo.mark_archived(id).await.unwrap();

    assert!(repo.list_by_category("ai").await.unwrap().is_empty());
}

fn artifact(kind: &str, key: &str) -> NewRawArtifact {
    NewRawArtifact {
        kind: kind.to_string(),
        artifact_key: key.to_string(),
        content_encoding: "utf8".to_string(),
        inline_body: b"body".to_vec(),
        byte_size: 4,
        sha256: format!("sha-{key}"),
        retention_policy: "always".to_string(),
        expires_at: None,
    }
}

async fn insert_entry_state(
    pool: &SqlitePool,
    source_id: i64,
    uid: &str,
    state: &str,
    created_at: OffsetDateTime,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            discovered_at, state, dedup_decision, created_at
        )
        VALUES (?, ?, ?, ?, 'title', ?, ?, 'fresh', ?)
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(uid)
    .bind(format!("https://example.com/{uid}"))
    .bind(format!("hash-{uid}"))
    .bind(created_at)
    .bind(state)
    .bind(created_at)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn count_state(pool: &SqlitePool, state: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM feed_entries WHERE state = ?")
        .bind(state)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn entry_link_hash(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT link_hash FROM feed_entries WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn insert_article_with_hash(pool: &SqlitePool, hash: &str) -> i64 {
    let rule = common::insert_rule(pool, "extractor", hash, &format!("sha-{hash}")).await;
    let source = common::seed_source(pool).await;
    let entry = common::insert_feed_entry(pool, source, hash, &format!("link-{hash}")).await;
    common::insert_article(pool, hash, entry, rule)
        .await
        .unwrap()
}

async fn article_hash(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT content_hash FROM articles WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn feed_source(
    category_key: &str,
    source_key: &str,
    status: FeedSourceStatus,
    config_version: i64,
) -> FeedSource {
    let now = OffsetDateTime::now_utc();
    FeedSource {
        id: 0,
        category_key: category_key.to_string(),
        source_key: source_key.to_string(),
        display_name: source_key.to_string(),
        feed_url: format!("https://example.com/{source_key}.xml"),
        feed_kind: FeedKind::Rss,
        status,
        priority: 1,
        etag: None,
        last_modified: None,
        last_fetched_at: None,
        last_success_at: None,
        consecutive_failures: 0,
        last_error: None,
        last_error_kind: None,
        config_version,
        created_at: now,
        updated_at: now,
    }
}
