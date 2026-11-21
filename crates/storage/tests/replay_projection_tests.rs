//! [`ReplayProjectionRepo`] on both dialects: the feed entry an
//! `html_payload` artifact is keyed by, its link, and the article it points
//! at (directly or as a content-dedup hit). The PostgreSQL case is
//! `#[ignore]` (needs docker).

mod common;

use common::{make_test_pool, pg::make_pg_test_pool};
use rss_ai_news_storage::{
    ArticleRepo, ArticleRepository, HtmlReplaySource, NewArticle, ReplayArticleDigest,
    ReplayProjectionRepo, ReplayProjectionRepository, StoragePool,
};
use time::OffsetDateTime;

async fn scalar(pool: &StoragePool, sql: &'static str, binds: &[&str]) -> i64 {
    match pool {
        StoragePool::Sqlite(pool) => {
            let mut query = sqlx::query_scalar::<_, i64>(sql);
            for bind in binds {
                query = query.bind(*bind);
            }
            query.fetch_one(pool).await.expect("sqlite fixture row")
        }
        StoragePool::Postgres(pool) => {
            let mut query = sqlx::query_scalar::<_, i64>(sql);
            for bind in binds {
                query = query.bind(*bind);
            }
            query.fetch_one(pool).await.expect("pg fixture row")
        }
    }
}

async fn entry(pool: &StoragePool, source: i64, uid: &str) -> i64 {
    let sql = "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
               title_raw, discovered_at, state, dedup_decision) \
               VALUES ($1, $2, $3, $4, 'title', $5, 'pending_fetch', 'fresh') RETURNING id";
    let link = format!("https://example.com/posts/{uid}/");
    let hash = format!("link-hash-{uid}");
    let now = OffsetDateTime::now_utc();
    match pool {
        StoragePool::Sqlite(pool) => sqlx::query_scalar::<_, i64>(sql)
            .bind(source)
            .bind(uid)
            .bind(&link)
            .bind(&hash)
            .bind(now)
            .fetch_one(pool)
            .await
            .expect("sqlite entry"),
        StoragePool::Postgres(pool) => sqlx::query_scalar::<_, i64>(sql)
            .bind(source)
            .bind(uid)
            .bind(&link)
            .bind(&hash)
            .bind(now)
            .fetch_one(pool)
            .await
            .expect("pg entry"),
    }
}

async fn link_article(pool: &StoragePool, article: i64, entry: i64) {
    let sql = "UPDATE feed_entries SET article_id = $1 WHERE id = $2";
    match pool {
        StoragePool::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(article)
                .bind(entry)
                .execute(pool)
                .await
                .expect("sqlite link");
        }
        StoragePool::Postgres(pool) => {
            sqlx::query(sql)
                .bind(article)
                .bind(entry)
                .execute(pool)
                .await
                .expect("pg link");
        }
    }
}

async fn assert_projection(pool: StoragePool) {
    let config = scalar(
        &pool,
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('config', 'cfg', 'c', 'sha-cfg', 'superseded') RETURNING id",
        &[],
    )
    .await;
    let extractor = scalar(
        &pool,
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', 'ext', 'e', 'sha-ext', 'superseded') RETURNING id",
        &[],
    )
    .await;
    let source = match &pool {
        StoragePool::Sqlite(sqlite) => sqlx::query_scalar::<_, i64>(
            "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
             feed_kind, config_version) VALUES ('ai', 'main', 'Main', \
             'https://example.com/feed.xml', 'rss', $1) RETURNING id",
        )
        .bind(config)
        .fetch_one(sqlite)
        .await
        .expect("sqlite source"),
        StoragePool::Postgres(pg) => sqlx::query_scalar::<_, i64>(
            "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
             feed_kind, config_version) VALUES ('ai', 'main', 'Main', \
             'https://example.com/feed.xml', 'rss', $1) RETURNING id",
        )
        .bind(config)
        .fetch_one(pg)
        .await
        .expect("pg source"),
    };
    let first = entry(&pool, source, "first").await;
    let dedup = entry(&pool, source, "dedup").await;
    let orphan = entry(&pool, source, "orphan").await;
    let article = ArticleRepo::new_with_storage(pool.clone())
        .insert_or_get_by_content_hash(&NewArticle {
            content_hash: "hash-a".to_string(),
            canonical_link: "https://example.com/canonical".to_string(),
            title: "Title A".to_string(),
            body_text: "body".to_string(),
            body_html_artifact_id: None,
            extractor_strategy: "readability".to_string(),
            extractor_version: extractor,
            content_quality: "high".to_string(),
            word_count: 42,
            origin_feed_entry_id: first,
        })
        .await
        .expect("article")
        .article_id;
    link_article(&pool, article, first).await;
    link_article(&pool, article, dedup).await;

    let repo = ReplayProjectionRepo::new_with_storage(pool);
    let digest = ReplayArticleDigest {
        id: article,
        title: "Title A".to_string(),
        word_count: 42,
        content_hash: "hash-a".to_string(),
    };
    for (entry, uid) in [(first, "first"), (dedup, "dedup")] {
        assert_eq!(
            repo.html_replay_source(entry).await.expect("projection"),
            Some(HtmlReplaySource {
                feed_entry_id: entry,
                normalized_link: format!("https://example.com/posts/{uid}/"),
                article: Some(digest.clone()),
            }),
        );
    }
    assert_eq!(
        repo.html_replay_source(orphan).await.expect("projection"),
        Some(HtmlReplaySource {
            feed_entry_id: orphan,
            normalized_link: "https://example.com/posts/orphan/".to_string(),
            article: None,
        }),
    );
    assert_eq!(
        repo.html_replay_source(orphan + 1_000)
            .await
            .expect("projection"),
        None
    );
}

#[tokio::test]
async fn sqlite_html_replay_source_reaches_entry_link_and_article() {
    let (_dir, pool) = make_test_pool().await;
    assert_projection(StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_html_replay_source_reaches_entry_link_and_article() {
    let ctx = make_pg_test_pool().await;
    assert_projection(ctx.storage_pool().clone()).await;
}
