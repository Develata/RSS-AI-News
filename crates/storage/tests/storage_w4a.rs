use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rss_ai_news_domain::{
    model::FeedSource,
    state::{FeedKind, FeedSourceStatus},
};
use rss_ai_news_storage::{
    FeedSourceRepo, FeedSourceRepository, RuleVersionRepo, StorageError, StoragePool,
    build_sqlite_pool, classify_db_error, run_migrations,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

static TEST_DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

async fn make_test_pool() -> (PathBuf, SqlitePool) {
    let counter = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    // F8-3 W4-3: PID + nanos + counter 三层叠加避免跨进程残留文件碰撞。
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rss-ai-news-storage-w4a-{}-{nanos}-{counter}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    let db_path = dir.join("test.sqlite");
    let pool = build_sqlite_pool(&db_path, 1, 5_000)
        .await
        .expect("test pool should be created");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrations should apply");
    (dir, pool)
}

async fn insert_rule(pool: &SqlitePool, kind: &str, tag: &str, sha: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO rule_versions (kind, version_tag, description, payload_sha256)
        VALUES (?, ?, 'test rule', ?)
        RETURNING id
        "#,
    )
    .bind(kind)
    .bind(tag)
    .bind(sha)
    .fetch_one(pool)
    .await
    .expect("rule version should insert")
}

async fn insert_feed_source(pool: &SqlitePool, config_version: i64) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind, config_version
        )
        VALUES ('ai', 'main', 'AI Main', 'https://example.com/feed.xml', 'rss', ?)
        RETURNING id
        "#,
    )
    .bind(config_version)
    .fetch_one(pool)
    .await
    .expect("feed source should insert")
}

async fn insert_feed_entry(pool: &SqlitePool, source_id: i64, uid: &str, link_hash: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw, discovered_at
        )
        VALUES (?, ?, 'https://example.com/article', ?, 'title', ?)
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(uid)
    .bind(link_hash)
    .bind(OffsetDateTime::now_utc())
    .fetch_one(pool)
    .await
    .expect("feed entry should insert")
}

async fn insert_article(
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
    .map_err(|error| classify_db_error(error, "articles", content_hash))
}

async fn seed_article(pool: &SqlitePool) -> (i64, i64, i64) {
    let rule_id = insert_rule(pool, "extractor", "v1", "extractor-sha").await;
    let config_id = insert_rule(pool, "config", "cfg", "cfg-sha").await;
    let source_id = insert_feed_source(pool, config_id).await;
    let entry_id = insert_feed_entry(pool, source_id, "uid-1", "link-hash-1").await;
    let article_id = insert_article(pool, "content-hash-1", entry_id, rule_id)
        .await
        .expect("article should insert");

    (rule_id, entry_id, article_id)
}

#[tokio::test]
async fn migrations_apply_from_empty_database() {
    let (_dir, _pool) = make_test_pool().await;
}

#[tokio::test]
async fn migrations_create_all_nine_tables() {
    let (_dir, pool) = make_test_pool().await;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE '_sqlx_%'",
    )
    .fetch_all(&pool)
    .await
    .expect("sqlite_master should be readable");
    let table_names = rows
        .into_iter()
        .map(|(name,)| name)
        .collect::<BTreeSet<_>>();

    for expected in [
        "feed_sources",
        "feed_entries",
        "articles",
        "article_ai_results",
        "publish_records",
        "publish_items",
        "raw_artifacts",
        "rule_versions",
        "run_events",
    ] {
        assert!(table_names.contains(expected), "missing table {expected}");
    }
}

#[tokio::test]
async fn articles_content_hash_unique_constraint_is_conflict() {
    let (_dir, pool) = make_test_pool().await;
    let (rule_id, _entry_id, _article_id) = seed_article(&pool).await;
    let source_id = sqlx::query_scalar::<_, i64>("SELECT id FROM feed_sources LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let second_entry_id = insert_feed_entry(&pool, source_id, "uid-2", "link-hash-2").await;

    let error = insert_article(&pool, "content-hash-1", second_entry_id, rule_id)
        .await
        .expect_err("duplicate content_hash should fail");
    assert!(matches!(error, StorageError::Conflict { .. }));
}

#[tokio::test]
async fn article_ai_results_unique_tuple_is_conflict() {
    let (_dir, pool) = make_test_pool().await;
    let (rule_id, _entry_id, article_id) = seed_article(&pool).await;
    let output_schema_id = insert_rule(&pool, "ai_output_schema", "v1", "schema-sha").await;

    for _ in 0..2 {
        let result = sqlx::query(
            r#"
            INSERT INTO article_ai_results (
                article_id, prompt_version, output_schema_version, model_id
            )
            VALUES (?, ?, ?, 'test-model')
            "#,
        )
        .bind(article_id)
        .bind(rule_id)
        .bind(output_schema_id)
        .execute(&pool)
        .await
        .map_err(|error| {
            classify_db_error(error, "article_ai_results", "article/prompt/schema/model")
        });

        if let Err(error) = result {
            assert!(matches!(error, StorageError::Conflict { .. }));
            return;
        }
    }

    panic!("duplicate article_ai_results tuple should fail");
}

#[tokio::test]
async fn publish_items_reject_ai_result_null_with_score() {
    let (_dir, pool) = make_test_pool().await;
    let (rule_id, _entry_id, article_id) = seed_article(&pool).await;
    let publish_record_id = insert_publish_record(&pool, rule_id).await;

    let error = insert_publish_item(&pool, publish_record_id, article_id, None, Some(10))
        .await
        .expect_err("half AI path should be rejected");
    assert!(matches!(error, StorageError::Sqlx(_)));
}

#[tokio::test]
async fn publish_items_reject_frozen_score_out_of_range() {
    let (_dir, pool) = make_test_pool().await;
    let (rule_id, _entry_id, article_id) = seed_article(&pool).await;
    let output_schema_id = insert_rule(&pool, "ai_output_schema", "v1", "schema-sha").await;
    let ai_result_id = insert_ai_result(&pool, article_id, rule_id, output_schema_id).await;
    let publish_record_id = insert_publish_record(&pool, rule_id).await;

    let error = insert_publish_item(
        &pool,
        publish_record_id,
        article_id,
        Some(ai_result_id),
        Some(150),
    )
    .await
    .expect_err("score > 100 should be rejected");
    assert!(matches!(error, StorageError::Sqlx(_)));
}

#[tokio::test]
async fn raw_artifacts_reject_inline_without_body() {
    let (_dir, pool) = make_test_pool().await;
    let error = sqlx::query(
        r#"
        INSERT INTO raw_artifacts (
            kind, artifact_key, content_encoding, storage_kind, byte_size, sha256, retention_policy
        )
        VALUES ('feed_payload', 'k1', 'utf8', 'inline', 0, 'sha', 'always')
        "#,
    )
    .execute(&pool)
    .await
    .map_err(|error| classify_db_error(error, "raw_artifacts", "storage_kind"))
    .expect_err("inline artifact without inline_body should be rejected");

    assert!(matches!(error, StorageError::Sqlx(_)));
}

#[tokio::test]
async fn feed_source_repo_upsert_and_find_by_id_round_trip() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_rule(&pool, "config", "cfg", "cfg-sha").await;
    let repo = FeedSourceRepo::new(pool);
    let now = OffsetDateTime::now_utc();
    let source = FeedSource {
        id: 0,
        category_key: "ai".to_string(),
        source_key: "main".to_string(),
        display_name: "AI Main".to_string(),
        feed_url: "https://example.com/feed.xml".to_string(),
        feed_kind: FeedKind::Rss,
        status: FeedSourceStatus::Active,
        priority: 10,
        etag: Some("etag".to_string()),
        last_modified: Some("Sat, 25 Apr 2026 00:00:00 GMT".to_string()),
        last_fetched_at: Some(now),
        last_success_at: Some(now),
        consecutive_failures: 0,
        last_error: None,
        last_error_kind: None,
        config_version: config_id,
        created_at: now,
        updated_at: now,
    };

    let id = repo.upsert(&source).await.expect("upsert should succeed");
    let found = repo
        .find_by_id(id)
        .await
        .expect("find should succeed")
        .expect("row should exist");

    assert_eq!(found.category_key, source.category_key);
    assert_eq!(found.source_key, source.source_key);
    assert_eq!(found.feed_kind, source.feed_kind);
    assert_eq!(found.status, source.status);
    assert_eq!(found.config_version, config_id);
}

#[tokio::test]
async fn feed_source_repo_find_by_keys_returns_none_when_missing() {
    let (_dir, pool) = make_test_pool().await;
    let repo = FeedSourceRepo::new(pool);

    let found = repo
        .find_by_keys("missing", "missing")
        .await
        .expect("find should succeed");

    assert!(found.is_none());
}

#[tokio::test]
async fn rule_version_config_get_or_create_returns_same_id_for_same_sha() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool);

    let left = repo
        .get_or_create_config_version_async("0123456789abcdef")
        .await
        .expect("first get_or_create should succeed");
    let right = repo
        .get_or_create_config_version_async("0123456789abcdef")
        .await
        .expect("second get_or_create should succeed");

    assert_eq!(left, right);
}

#[tokio::test]
async fn rule_version_config_get_or_create_returns_different_ids_for_different_sha() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool);

    let left = repo
        .get_or_create_config_version_async("aaaaaaaaaaaabbbb")
        .await
        .expect("first get_or_create should succeed");
    let right = repo
        .get_or_create_config_version_async("bbbbbbbbbbbbcccc")
        .await
        .expect("second get_or_create should succeed");

    assert_ne!(left, right);
}

#[tokio::test]
async fn config_version_store_trait_impl_round_trip() {
    use rss_ai_news_config::ConfigVersionStore;

    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool);
    let store: &dyn ConfigVersionStore = &repo;

    let first = store
        .get_or_create_config_version("ccccccccccccdddd")
        .await
        .expect("first trait call should succeed");
    let second = store
        .get_or_create_config_version("ccccccccccccdddd")
        .await
        .expect("second trait call should succeed");

    assert_eq!(first, second);
}

#[tokio::test]
async fn down_sql_then_up_sql_succeeds() {
    let (_dir, pool) = make_test_pool().await;

    sqlx::raw_sql(include_str!(
        "../../../migrations/sqlite/0001_init.down.sql"
    ))
    .execute(&pool)
    .await
    .expect("down migration sql should run");
    sqlx::raw_sql(include_str!("../../../migrations/sqlite/0001_init.up.sql"))
        .execute(&pool)
        .await
        .expect("up migration sql should run after down");
}

async fn insert_publish_record(pool: &SqlitePool, rule_id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO publish_records (
            idempotency_key, category_key, report_date, target_timezone,
            render_version, selection_policy_version
        )
        VALUES ('ai-2026-04-25-v1', 'ai', '2026-04-25', 'Asia/Shanghai', ?, ?)
        RETURNING id
        "#,
    )
    .bind(rule_id)
    .bind(rule_id)
    .fetch_one(pool)
    .await
    .expect("publish record should insert")
}

async fn insert_ai_result(
    pool: &SqlitePool,
    article_id: i64,
    prompt_version: i64,
    output_schema_version: i64,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO article_ai_results (
            article_id, prompt_version, output_schema_version, model_id
        )
        VALUES (?, ?, ?, 'test-model')
        RETURNING id
        "#,
    )
    .bind(article_id)
    .bind(prompt_version)
    .bind(output_schema_version)
    .fetch_one(pool)
    .await
    .expect("AI result should insert")
}

async fn insert_publish_item(
    pool: &SqlitePool,
    publish_record_id: i64,
    article_id: i64,
    article_ai_result_id: Option<i64>,
    frozen_score: Option<i64>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"
        INSERT INTO publish_items (
            publish_record_id, position, article_id, article_ai_result_id,
            frozen_title, frozen_summary, frozen_tags_json, frozen_score,
            frozen_canonical_link, frozen_source_display_name
        )
        VALUES (?, 1, ?, ?, 'title', 'summary', '[]', ?, 'https://example.com/article', 'AI Main')
        "#,
    )
    .bind(publish_record_id)
    .bind(article_id)
    .bind(article_ai_result_id)
    .bind(frozen_score)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| classify_db_error(error, "publish_items", "checks"))
}
