mod common;

use rss_ai_news_storage::{
    ArticleAiResultRepository, FeedEntryRepository, NewAiResult, NewFeedEntry,
    SqliteArticleAiResultRepo, SqliteFeedEntryRepo, StorageError,
};
use time::OffsetDateTime;

use common::{
    insert_article, insert_feed_entry, insert_rule, make_test_pool, seed_article, seed_source,
};

#[tokio::test]
async fn feed_entry_uid_unique_duplicate_returns_none() {
    let (_dir, pool) = make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let repo = SqliteFeedEntryRepo::new(pool);
    let entry = new_feed_entry(source_id, "uid-1", "link-hash-1");

    let first = repo
        .insert_if_new(&entry)
        .await
        .expect("first insert should succeed");
    let second = repo
        .insert_if_new(&entry)
        .await
        .expect("duplicate insert should be idempotent");

    assert!(first.is_some());
    assert!(second.is_none());
}

#[tokio::test]
async fn feed_entry_link_hash_lookup_distinguishes_hit_and_miss() {
    let (_dir, pool) = make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let repo = SqliteFeedEntryRepo::new(pool);

    repo.insert_if_new(&new_feed_entry(source_id, "uid-1", "hash-hit"))
        .await
        .expect("insert should succeed");

    assert!(
        repo.exists_by_link_hash("hash-hit")
            .await
            .expect("lookup should succeed")
    );
    assert!(
        !repo
            .exists_by_link_hash("hash-miss")
            .await
            .expect("lookup should succeed")
    );
}

#[tokio::test]
async fn articles_content_hash_duplicate_is_conflict() {
    let (_dir, pool) = make_test_pool().await;
    let (rule_id, _entry_id, _article_id) = seed_article(&pool).await;
    let source_id = seed_source(&pool).await;
    let second_entry_id = insert_feed_entry(&pool, source_id, "uid-2", "link-hash-2").await;

    let error = insert_article(&pool, "content-hash-article", second_entry_id, rule_id)
        .await
        .expect_err("duplicate content_hash should fail");

    assert!(matches!(
        error,
        StorageError::Conflict { ref table, .. } if table == "articles"
    ));
}

#[tokio::test]
async fn ai_result_unique_tuple_duplicate_returns_none_via_repo() {
    let (_dir, pool) = make_test_pool().await;
    let (prompt_id, _entry_id, article_id) = seed_article(&pool).await;
    let schema_id = insert_rule(&pool, "ai_output_schema", "v1", "schema-sha").await;
    let repo = SqliteArticleAiResultRepo::new(pool);
    let item = NewAiResult {
        article_id,
        prompt_version: prompt_id,
        output_schema_version: schema_id,
        model_id: "test-model".to_string(),
    };

    let first = repo
        .insert_pending(&item)
        .await
        .expect("first insert should succeed");
    let second = repo
        .insert_pending(&item)
        .await
        .expect("duplicate insert should be idempotent");

    assert!(first.is_some());
    assert!(second.is_none());
}

fn new_feed_entry(source_id: i64, uid: &str, link_hash: &str) -> NewFeedEntry {
    NewFeedEntry {
        source_id,
        feed_entry_uid: uid.to_string(),
        normalized_link: format!("https://example.com/{uid}"),
        link_hash: link_hash.to_string(),
        title_raw: "title".to_string(),
        summary_raw: None,
        published_at: None,
        discovered_at: OffsetDateTime::now_utc(),
    }
}
