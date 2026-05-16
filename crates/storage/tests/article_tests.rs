mod common;

use rss_ai_news_domain::state::{ArticleState, ContentQuality, ExtractorStrategy};
use rss_ai_news_storage::{ArticleRepo, ArticleRepository, NewArticle};

use common::{insert_feed_entry, insert_rule, make_test_pool, seed_source};

#[tokio::test]
async fn insert_writes_new_article_returns_newly_created_true() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, entry_id, rule_id) = setup(&pool).await;

    let outcome = repo
        .insert_or_get_by_content_hash(&new_article("content-hash-new", entry_id, rule_id))
        .await
        .expect("article insert should succeed");

    assert!(outcome.newly_created);
    assert!(outcome.article_id > 0);
}

#[tokio::test]
async fn insert_with_existing_content_hash_returns_existing_id_newly_created_false() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, entry_id, rule_id) = setup(&pool).await;
    let first = repo
        .insert_or_get_by_content_hash(&new_article("content-hash-dup", entry_id, rule_id))
        .await
        .expect("first article insert should succeed");
    let source_id = seed_source(&pool).await;
    let second_entry_id = insert_feed_entry(&pool, source_id, "uid-2", "link-hash-2").await;

    let second = repo
        .insert_or_get_by_content_hash(&new_article("content-hash-dup", second_entry_id, rule_id))
        .await
        .expect("duplicate article insert should be idempotent");

    assert!(first.newly_created);
    assert!(!second.newly_created);
    assert_eq!(second.article_id, first.article_id);
}

#[tokio::test]
async fn find_by_id_returns_inserted_article() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, entry_id, rule_id) = setup(&pool).await;
    let outcome = repo
        .insert_or_get_by_content_hash(&new_article("content-hash-find", entry_id, rule_id))
        .await
        .expect("article insert should succeed");

    let article = repo
        .find_by_id(outcome.article_id)
        .await
        .expect("article lookup should succeed")
        .expect("article should exist");

    assert_eq!(article.content_hash, "content-hash-find");
    assert_eq!(article.extractor_strategy, ExtractorStrategy::Readability);
    assert_eq!(article.content_quality, ContentQuality::High);
    assert_eq!(article.state, ArticleState::Persisted);
}

#[tokio::test]
async fn find_by_id_returns_none_when_missing() {
    let (_dir, pool) = make_test_pool().await;
    let repo = ArticleRepo::new(pool);

    let article = repo
        .find_by_id(9_999)
        .await
        .expect("missing article lookup should succeed");

    assert!(article.is_none());
}

async fn setup(pool: &sqlx::SqlitePool) -> (ArticleRepo, i64, i64) {
    let rule_id = insert_rule(pool, "extractor", "article-test", "article-test-sha").await;
    let source_id = seed_source(pool).await;
    let entry_id = insert_feed_entry(pool, source_id, "uid-1", "link-hash-1").await;
    (ArticleRepo::new(pool.clone()), entry_id, rule_id)
}

fn new_article(content_hash: &str, entry_id: i64, rule_id: i64) -> NewArticle {
    NewArticle {
        content_hash: content_hash.to_string(),
        canonical_link: "https://example.com/article".to_string(),
        title: "title".to_string(),
        body_text: "body text".to_string(),
        body_html_artifact_id: None,
        extractor_strategy: "readability".to_string(),
        extractor_version: rule_id,
        content_quality: "high".to_string(),
        word_count: 2,
        origin_feed_entry_id: entry_id,
    }
}
