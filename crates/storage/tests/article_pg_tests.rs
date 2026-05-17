//! W11-P3-C-3：[`ArticleRepo`] PG 分支冒烟。
//!
//! 覆盖：
//!   - `insert_or_get_by_content_hash` 两条路径：新行 (newly_created=true) /
//!     已存在 hash 走兜底 SELECT (newly_created=false)
//!   - `peek_content_hash_outcome` 三态：Unchanged / Conflict / Updated
//!     （PG 上 `CASE WHEN EXISTS(...) THEN 1 ELSE 0 END` decode `i32` 工作）
//!   - `update_content_hash` 真做 UPDATE + 行存在 → Updated
//!
//! 默认 `#[ignore]`，需要 docker。

mod common;

use common::pg::{PgTestContext, make_pg_test_pool};
use rss_ai_news_storage::{ArticleRepo, ArticleRepository, NewArticle, UpdateContentHashOutcome};
use time::OffsetDateTime;

async fn seed_feed_source(ctx: &PgTestContext) -> (i64, i64) {
    let rule_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('config', 'cfg-1', 'c', 'sha-cfg', 'superseded') RETURNING id",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
            feed_kind, config_version) \
         VALUES ('ai', 'main', 'AI Main', 'https://example.com/feed.xml', 'rss', $1) \
         RETURNING id",
    )
    .bind(rule_id)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    (rule_id, source_id)
}

async fn seed_feed_entry(ctx: &PgTestContext, source_id: i64, uid: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
            title_raw, discovered_at, state, dedup_decision) \
         VALUES ($1, $2, $3, $4, 'title', $5, 'pending_fetch', 'fresh') RETURNING id",
    )
    .bind(source_id)
    .bind(uid)
    .bind(format!("https://example.com/{uid}"))
    .bind(format!("link-hash-{uid}"))
    .bind(OffsetDateTime::now_utc())
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap()
}

async fn seed_extractor_rule(ctx: &PgTestContext, tag: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', $1, 'ext', $2, 'superseded') RETURNING id",
    )
    .bind(tag)
    .bind(format!("sha-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap()
}

fn new_article(
    content_hash: &str,
    origin_feed_entry_id: i64,
    extractor_version: i64,
) -> NewArticle {
    NewArticle {
        content_hash: content_hash.to_string(),
        canonical_link: "https://example.com/article".to_string(),
        title: "Article".to_string(),
        body_text: "body text body text".to_string(),
        body_html_artifact_id: None,
        extractor_strategy: "readability".to_string(),
        extractor_version,
        content_quality: "high".to_string(),
        word_count: 4,
        origin_feed_entry_id,
    }
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_insert_or_get_by_content_hash_returns_existing_on_second_call() {
    let ctx = make_pg_test_pool().await;
    let (_, source_id) = seed_feed_source(&ctx).await;
    let entry_id = seed_feed_entry(&ctx, source_id, "uid-a").await;
    let extractor_id = seed_extractor_rule(&ctx, "ext-v1").await;
    let repo = ArticleRepo::new_with_storage(ctx.storage_pool().clone());

    let article = new_article("hash-1", entry_id, extractor_id);
    let first = repo
        .insert_or_get_by_content_hash(&article)
        .await
        .expect("pg insert first");
    assert!(first.newly_created, "first call inserts a new row");
    let id = first.article_id;

    let second = repo
        .insert_or_get_by_content_hash(&article)
        .await
        .expect("pg insert second");
    assert!(
        !second.newly_created,
        "second call hits ON CONFLICT DO NOTHING"
    );
    assert_eq!(second.article_id, id, "fallback SELECT returns the same id");

    let fetched = repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(fetched.content_hash, "hash-1");
    assert_eq!(fetched.origin_feed_entry_id, entry_id);
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_peek_content_hash_outcome_three_states() {
    let ctx = make_pg_test_pool().await;
    let (_, source_id) = seed_feed_source(&ctx).await;
    let entry_a = seed_feed_entry(&ctx, source_id, "uid-peek-a").await;
    let entry_b = seed_feed_entry(&ctx, source_id, "uid-peek-b").await;
    let extractor_id = seed_extractor_rule(&ctx, "ext-peek").await;
    let repo = ArticleRepo::new_with_storage(ctx.storage_pool().clone());

    let article_a = new_article("hash-A", entry_a, extractor_id);
    let article_b = new_article("hash-B", entry_b, extractor_id);
    let id_a = repo
        .insert_or_get_by_content_hash(&article_a)
        .await
        .unwrap()
        .article_id;
    let _id_b = repo
        .insert_or_get_by_content_hash(&article_b)
        .await
        .unwrap()
        .article_id;

    // Unchanged：旧 hash 等于新 hash
    let unchanged = repo
        .peek_content_hash_outcome(id_a, "hash-A")
        .await
        .expect("pg peek unchanged");
    assert_eq!(unchanged, UpdateContentHashOutcome::Unchanged);

    // Conflict：新 hash 已被另一行占用（hash-B 已存在）
    let conflict = repo
        .peek_content_hash_outcome(id_a, "hash-B")
        .await
        .expect("pg peek conflict (EXISTS hit)");
    assert_eq!(conflict, UpdateContentHashOutcome::Conflict);

    // Updated：新 hash 不冲突
    let updated = repo
        .peek_content_hash_outcome(id_a, "hash-NEW")
        .await
        .expect("pg peek updated");
    assert_eq!(updated, UpdateContentHashOutcome::Updated);

    // 没有真正 UPDATE：peek 是 dry-run
    let still_a = repo.find_by_id(id_a).await.unwrap().unwrap();
    assert_eq!(still_a.content_hash, "hash-A", "peek must not mutate");
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_update_content_hash_then_find_returns_new_hash() {
    let ctx = make_pg_test_pool().await;
    let (_, source_id) = seed_feed_source(&ctx).await;
    let entry_id = seed_feed_entry(&ctx, source_id, "uid-update").await;
    let extractor_id = seed_extractor_rule(&ctx, "ext-update").await;
    let repo = ArticleRepo::new_with_storage(ctx.storage_pool().clone());

    let article = new_article("hash-old", entry_id, extractor_id);
    let id = repo
        .insert_or_get_by_content_hash(&article)
        .await
        .unwrap()
        .article_id;

    let outcome = repo
        .update_content_hash(id, "hash-new")
        .await
        .expect("pg update_content_hash");
    assert_eq!(outcome, UpdateContentHashOutcome::Updated);

    let after = repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(after.content_hash, "hash-new");
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_list_persisted_for_ai_task_gen_paginates_by_id() {
    let ctx = make_pg_test_pool().await;
    let (_, source_id) = seed_feed_source(&ctx).await;
    let extractor_id = seed_extractor_rule(&ctx, "ext-list").await;
    let repo = ArticleRepo::new_with_storage(ctx.storage_pool().clone());

    let mut ids = Vec::new();
    for i in 0..3 {
        let entry_id = seed_feed_entry(&ctx, source_id, &format!("uid-list-{i}")).await;
        let id = repo
            .insert_or_get_by_content_hash(&new_article(
                &format!("hash-list-{i}"),
                entry_id,
                extractor_id,
            ))
            .await
            .unwrap()
            .article_id;
        ids.push(id);
    }

    // batch_size=2 + after_id=0 → 拿 ids[0..2]
    let page1 = repo
        .list_persisted_for_ai_task_gen(2, 0)
        .await
        .expect("pg list page1");
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].article_id, ids[0]);
    assert_eq!(page1[1].article_id, ids[1]);

    // after_id=ids[1] → 拿 ids[2..]
    let page2 = repo
        .list_persisted_for_ai_task_gen(2, ids[1])
        .await
        .expect("pg list page2");
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].article_id, ids[2]);
}
