#![allow(dead_code)]

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rss_ai_news_storage::{
    StorageError, StoragePool, build_sqlite_pool, classify_sqlite_error, run_migrations,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

static TEST_DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub async fn make_test_pool_with_connections(max_connections: u32) -> (PathBuf, SqlitePool) {
    let counter = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    // F8-3 W4-3: PID + nanos + counter 三层叠加避免跨进程残留文件碰撞。
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rss-ai-news-storage-w4b-{}-{nanos}-{counter}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    let db_path = dir.join("test.sqlite");
    let pool = build_sqlite_pool(&db_path, max_connections, 5_000)
        .await
        .expect("test pool should be created");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrations should apply");
    (dir, pool)
}

pub async fn make_test_pool() -> (PathBuf, SqlitePool) {
    make_test_pool_with_connections(1).await
}

pub async fn insert_rule(pool: &SqlitePool, kind: &str, tag: &str, sha: &str) -> i64 {
    // F15-1 引入 partial unique index `uq_rule_versions_kind_active`。fixture
    // 调用方常重复传同 kind，需要避免触发；用 status='superseded' 写入
    // 仅服务 fixture 的外键引用语义，不参与 active_rule 真值（后者由
    // rule_version_active_tests 单独锁定）。
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) VALUES (?, ?, 'test rule', ?, 'superseded') RETURNING id",
    )
    .bind(kind)
    .bind(tag)
    .bind(sha)
    .fetch_one(pool)
    .await
    .expect("rule version should insert")
}

pub async fn insert_feed_source(pool: &SqlitePool, config_version: i64) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind, config_version
        )
        VALUES ('ai', ?, 'AI Main', 'https://example.com/feed.xml', 'rss', ?)
        RETURNING id
        "#,
    )
    .bind(format!(
        "main-{}",
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ))
    .bind(config_version)
    .fetch_one(pool)
    .await
    .expect("feed source should insert")
}

pub async fn seed_source(pool: &SqlitePool) -> i64 {
    let config_id = insert_rule(
        pool,
        "config",
        &format!("cfg-{}", OffsetDateTime::now_utc().unix_timestamp_nanos()),
        &format!(
            "cfg-sha-{}",
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ),
    )
    .await;
    insert_feed_source(pool, config_id).await
}

pub async fn insert_feed_entry(
    pool: &SqlitePool,
    source_id: i64,
    uid: &str,
    link_hash: &str,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            discovered_at, state, dedup_decision
        )
        VALUES (?, ?, ?, ?, 'title', ?, 'pending_fetch', 'fresh')
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(uid)
    .bind(format!("https://example.com/{uid}"))
    .bind(link_hash)
    .bind(OffsetDateTime::now_utc())
    .fetch_one(pool)
    .await
    .expect("feed entry should insert")
}

pub async fn insert_article(
    pool: &SqlitePool,
    content_hash: &str,
    origin_feed_entry_id: i64,
    extractor_version: i64,
) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO articles (
            content_hash, canonical_link, title, body_text, extractor_strategy,
            extractor_version, content_quality, origin_feed_entry_id
        )
        VALUES (?, 'https://example.com/article', 'title', 'body', 'readability', ?, 'high', ?)
        RETURNING id
        "#,
    )
    .bind(content_hash)
    .bind(extractor_version)
    .bind(origin_feed_entry_id)
    .fetch_one(pool)
    .await
    .map_err(|error| classify_sqlite_error(error, "articles", content_hash))
}

pub async fn seed_article(pool: &SqlitePool) -> (i64, i64, i64) {
    let rule_id = insert_rule(
        pool,
        "extractor",
        &format!(
            "extractor-{}",
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ),
        &format!(
            "extractor-sha-{}",
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ),
    )
    .await;
    let source_id = seed_source(pool).await;
    let entry_id = insert_feed_entry(pool, source_id, "uid-article", "link-hash-article").await;
    let article_id = insert_article(pool, "content-hash-article", entry_id, rule_id)
        .await
        .expect("article should insert");
    (rule_id, entry_id, article_id)
}
