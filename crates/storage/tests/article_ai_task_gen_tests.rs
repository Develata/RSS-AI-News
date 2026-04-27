mod common;

use rss_ai_news_storage::{ArticleRepository, SqliteArticleRepo};

use common::{insert_article, insert_feed_entry, insert_rule, make_test_pool, seed_source};

#[tokio::test]
async fn list_persisted_filters_state_persisted_orders_by_id() {
    let (_dir, pool) = make_test_pool().await;
    let rule_id = insert_rule(&pool, "extractor", "extractor-task-gen-1", "sha-task-gen-1").await;
    let source_id = seed_source(&pool).await;
    let first_entry = insert_feed_entry(&pool, source_id, "uid-1", "link-hash-1").await;
    let second_entry = insert_feed_entry(&pool, source_id, "uid-2", "link-hash-2").await;
    let third_entry = insert_feed_entry(&pool, source_id, "uid-3", "link-hash-3").await;
    let first = insert_article(&pool, "content-hash-1", first_entry, rule_id)
        .await
        .expect("first article should insert");
    let second = insert_article(&pool, "content-hash-2", second_entry, rule_id)
        .await
        .expect("second article should insert");
    let third = insert_article(&pool, "content-hash-3", third_entry, rule_id)
        .await
        .expect("third article should insert");
    sqlx::query("UPDATE articles SET state = 'ai_pending' WHERE id = ?")
        .bind(second)
        .execute(&pool)
        .await
        .expect("article state should update");

    let repo = SqliteArticleRepo::new(pool);
    let candidates = repo
        .list_persisted_for_ai_task_gen(10, 0)
        .await
        .expect("list should succeed");

    let ids = candidates
        .iter()
        .map(|candidate| candidate.article_id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![first, third]);
    assert_eq!(candidates[0].title, "title");
    assert_eq!(candidates[0].body_text, "body");
}

#[tokio::test]
async fn list_persisted_paginates_by_after_id() {
    let (_dir, pool) = make_test_pool().await;
    let rule_id = insert_rule(&pool, "extractor", "extractor-task-gen-2", "sha-task-gen-2").await;
    let source_id = seed_source(&pool).await;
    let first_entry = insert_feed_entry(&pool, source_id, "uid-1", "link-hash-1").await;
    let second_entry = insert_feed_entry(&pool, source_id, "uid-2", "link-hash-2").await;
    let third_entry = insert_feed_entry(&pool, source_id, "uid-3", "link-hash-3").await;
    let first = insert_article(&pool, "content-hash-a", first_entry, rule_id)
        .await
        .expect("first article should insert");
    let second = insert_article(&pool, "content-hash-b", second_entry, rule_id)
        .await
        .expect("second article should insert");
    let third = insert_article(&pool, "content-hash-c", third_entry, rule_id)
        .await
        .expect("third article should insert");

    let repo = SqliteArticleRepo::new(pool);
    let candidates = repo
        .list_persisted_for_ai_task_gen(1, first)
        .await
        .expect("list should succeed");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].article_id, second);

    let candidates = repo
        .list_persisted_for_ai_task_gen(10, second)
        .await
        .expect("list should succeed");
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.article_id)
            .collect::<Vec<_>>(),
        vec![third]
    );
}

#[tokio::test]
async fn list_persisted_returns_empty_when_no_candidates() {
    let (_dir, pool) = make_test_pool().await;
    let rule_id = insert_rule(&pool, "extractor", "extractor-task-gen-3", "sha-task-gen-3").await;
    let source_id = seed_source(&pool).await;
    let entry = insert_feed_entry(&pool, source_id, "uid-1", "link-hash-1").await;
    let article_id = insert_article(&pool, "content-hash-empty", entry, rule_id)
        .await
        .expect("article should insert");
    sqlx::query("UPDATE articles SET state = 'ai_pending' WHERE id = ?")
        .bind(article_id)
        .execute(&pool)
        .await
        .expect("article state should update");

    let repo = SqliteArticleRepo::new(pool);
    let candidates = repo
        .list_persisted_for_ai_task_gen(10, 0)
        .await
        .expect("list should succeed");

    assert!(candidates.is_empty());
}
