mod common;

use rss_ai_news_storage::{PublishItemRepository, SqlitePublishItemRepo};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_rule, make_test_pool};

#[tokio::test]
async fn select_ai_path_returns_only_ready_for_publish_with_keep_and_min_score() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqlitePublishItemRepo::new(pool.clone());
    let keep = seed_ai_article(&pool, "ai", "keep", "ready_for_publish", 90, 1).await;
    seed_ai_article(&pool, "ai", "low", "ready_for_publish", 20, 1).await;
    seed_ai_article(&pool, "ai", "filtered", "ready_for_publish", 95, 0).await;
    seed_ai_article(&pool, "ai", "persisted", "persisted", 99, 1).await;

    let rows = repo
        .select_ai_path_candidates("ai", 50, 10)
        .await
        .expect("selection should succeed");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].article_id, keep);
    assert_eq!(rows[0].importance_score, Some(90));
}

#[tokio::test]
async fn select_ai_path_orders_by_score_desc_then_created_at_desc() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqlitePublishItemRepo::new(pool.clone());
    let older = seed_ai_article(&pool, "ai", "older", "ready_for_publish", 80, 1).await;
    let newer = seed_ai_article(&pool, "ai", "newer", "ready_for_publish", 80, 1).await;
    let top = seed_ai_article(&pool, "ai", "top", "ready_for_publish", 95, 1).await;
    set_article_created_at(&pool, older, OffsetDateTime::now_utc() - Duration::hours(2)).await;
    set_article_created_at(&pool, newer, OffsetDateTime::now_utc()).await;

    let ids = repo
        .select_ai_path_candidates("ai", 0, 10)
        .await
        .expect("selection should succeed")
        .into_iter()
        .map(|row| row.article_id)
        .collect::<Vec<_>>();

    assert_eq!(ids, vec![top, newer, older]);
}

#[tokio::test]
async fn select_ai_path_filters_by_category_key() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqlitePublishItemRepo::new(pool.clone());
    let ai = seed_ai_article(&pool, "ai", "ai-cat", "ready_for_publish", 90, 1).await;
    seed_ai_article(&pool, "ml", "ml-cat", "ready_for_publish", 95, 1).await;

    let rows = repo
        .select_ai_path_candidates("ai", 0, 10)
        .await
        .expect("selection should succeed");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].article_id, ai);
    assert_eq!(rows[0].category_key, "ai");
}

#[tokio::test]
async fn select_ai_off_passthrough_returns_persisted_articles_without_any_ai_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqlitePublishItemRepo::new(pool.clone());
    let article = seed_passthrough_article(&pool, "ai", "direct", "persisted").await;

    let rows = repo
        .select_ai_off_passthrough_candidates("ai", 10)
        .await
        .expect("selection should succeed");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].article_id, article);
    assert_eq!(rows[0].article_ai_result_id, None);
    assert_eq!(rows[0].importance_score, None);
    assert_eq!(rows[0].summary, "summary direct");
    assert_eq!(rows[0].tags_json, "[]");
}

#[tokio::test]
async fn select_ai_off_passthrough_excludes_articles_with_filtered_or_permanent_failed_ai_rows() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqlitePublishItemRepo::new(pool.clone());
    let clean = seed_passthrough_article(&pool, "ai", "clean", "persisted").await;
    let filtered = seed_passthrough_article(&pool, "ai", "filtered-direct", "persisted").await;
    let failed = seed_passthrough_article(&pool, "ai", "failed-direct", "persisted").await;
    insert_ai_row(&pool, filtered, "filtered", None, 0).await;
    insert_ai_row(&pool, failed, "permanent_failed", None, 1).await;

    let rows = repo
        .select_ai_off_passthrough_candidates("ai", 10)
        .await
        .expect("selection should succeed");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].article_id, clean);
}

async fn seed_ai_article(
    pool: &SqlitePool,
    category_key: &str,
    content_hash: &str,
    state: &str,
    score: i32,
    keep: i32,
) -> i64 {
    let article_id =
        seed_article(pool, category_key, content_hash, state, Some("summary raw")).await;
    insert_ai_row(pool, article_id, "succeeded", Some(score), keep).await;
    article_id
}

async fn seed_passthrough_article(
    pool: &SqlitePool,
    category_key: &str,
    content_hash: &str,
    state: &str,
) -> i64 {
    seed_article(
        pool,
        category_key,
        content_hash,
        state,
        Some(&format!("summary {content_hash}")),
    )
    .await
}

async fn seed_article(
    pool: &SqlitePool,
    category_key: &str,
    content_hash: &str,
    state: &str,
    summary_raw: Option<&str>,
) -> i64 {
    let config = insert_rule(pool, "config", &format!("cfg-{content_hash}"), content_hash).await;
    let extractor = insert_rule(
        pool,
        "extractor",
        &format!("extractor-{content_hash}"),
        &format!("extractor-sha-{content_hash}"),
    )
    .await;
    let source_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind, config_version
        )
        VALUES (?, ?, ?, ?, 'rss', ?)
        RETURNING id
        "#,
    )
    .bind(category_key)
    .bind(format!("source-{content_hash}"))
    .bind(format!("Source {content_hash}"))
    .bind(format!("https://example.com/{content_hash}.xml"))
    .bind(config)
    .fetch_one(pool)
    .await
    .expect("source should insert");
    let entry_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            summary_raw, discovered_at, state, dedup_decision
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, 'persisted', 'fresh')
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(format!("uid-{content_hash}"))
    .bind(format!("https://example.com/{content_hash}"))
    .bind(format!("link-{content_hash}"))
    .bind(format!("title raw {content_hash}"))
    .bind(summary_raw)
    .bind(OffsetDateTime::now_utc())
    .fetch_one(pool)
    .await
    .expect("entry should insert");
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO articles (
            content_hash, canonical_link, title, body_text, extractor_strategy,
            extractor_version, content_quality, word_count, origin_feed_entry_id, state
        )
        VALUES (?, ?, ?, ?, 'readability', ?, 'high', 2, ?, ?)
        RETURNING id
        "#,
    )
    .bind(content_hash)
    .bind(format!("https://example.com/article/{content_hash}"))
    .bind(format!("title {content_hash}"))
    .bind(format!("body {content_hash}"))
    .bind(extractor)
    .bind(entry_id)
    .bind(state)
    .fetch_one(pool)
    .await
    .expect("article should insert")
}

async fn insert_ai_row(
    pool: &SqlitePool,
    article_id: i64,
    state: &str,
    score: Option<i32>,
    keep: i32,
) -> i64 {
    let prompt = insert_rule(
        pool,
        "prompt",
        &format!("prompt-{article_id}-{state}"),
        "prompt-sha",
    )
    .await;
    let schema = insert_rule(
        pool,
        "ai_output_schema",
        &format!("schema-{article_id}-{state}"),
        "schema-sha",
    )
    .await;
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO article_ai_results (
            article_id, prompt_version, output_schema_version, model_id, state,
            summary, tags_json, importance_score, keep_decision
        )
        VALUES (?, ?, ?, ?, ?, 'ai summary', '["ai"]', ?, ?)
        RETURNING id
        "#,
    )
    .bind(article_id)
    .bind(prompt)
    .bind(schema)
    .bind(format!("model-{state}"))
    .bind(state)
    .bind(score)
    .bind(keep)
    .fetch_one(pool)
    .await
    .expect("AI row should insert")
}

async fn set_article_created_at(pool: &SqlitePool, article_id: i64, created_at: OffsetDateTime) {
    sqlx::query("UPDATE articles SET created_at = ? WHERE id = ?")
        .bind(created_at)
        .bind(article_id)
        .execute(pool)
        .await
        .expect("article timestamp should update");
}
