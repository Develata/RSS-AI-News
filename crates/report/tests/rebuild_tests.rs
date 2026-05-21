use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rss_ai_news_report::{
    RenderConfig, RenderTemplates, ReportError, rebuild_markdown, render_markdown,
};
use rss_ai_news_storage::{
    PublishItemRepo, PublishItemRepository, PublishRecordRepo, StoragePool, build_sqlite_pool,
    run_migrations,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

static TEST_DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[tokio::test]
async fn rebuild_returns_byte_equal_markdown_when_render_config_matches() {
    let (_dir, pool) = make_pool().await;
    let record_id = seed_snapshot(&pool).await;
    let record_repo = PublishRecordRepo::new(pool.clone());
    let item_repo = PublishItemRepo::new(pool);
    let items = item_repo.list_by_publish_record(record_id).await.unwrap();
    let frozen = items
        .into_iter()
        .map(|item| {
            rss_ai_news_domain::dto::publish::FrozenPublishItem::try_new(
                u32::try_from(item.position).unwrap(),
                item.article_id,
                item.article_ai_result_id,
                item.frozen_title,
                item.frozen_summary,
                item.frozen_tags_json,
                item.frozen_score,
                item.frozen_canonical_link,
                item.frozen_source_display_name,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let config = render_config();
    let original = render_markdown(record_id, "ai", "2026-04-28", &frozen, &config).unwrap();

    let rebuilt = rebuild_markdown(&record_repo, &item_repo, record_id, &config)
        .await
        .unwrap();

    assert_eq!(rebuilt.markdown_content, original.markdown_content);
    assert_eq!(rebuilt.relative_path, original.relative_path);
    assert_eq!(rebuilt.publish_record_id, original.publish_record_id);
    assert_eq!(rebuilt.category_key, original.category_key);
    assert_eq!(rebuilt.report_date, original.report_date);
}

#[tokio::test]
async fn rebuild_returns_error_when_publish_record_missing() {
    let (_dir, pool) = make_pool().await;
    let record_repo = PublishRecordRepo::new(pool.clone());
    let item_repo = PublishItemRepo::new(pool);

    let error = rebuild_markdown(&record_repo, &item_repo, 999, &render_config())
        .await
        .unwrap_err();

    assert!(matches!(error, ReportError::RenderFailed(_)));
}

async fn make_pool() -> (PathBuf, SqlitePool) {
    let counter = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    // F8-3 W4-3: PID + nanos + counter 三层叠加避免跨进程残留文件碰撞。
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rss-ai-news-report-rebuild-{}-{nanos}-{counter}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = build_sqlite_pool(&dir.join("test.sqlite"), 1, 5_000)
        .await
        .unwrap();
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .unwrap();
    (dir, pool)
}

async fn seed_snapshot(pool: &SqlitePool) -> i64 {
    let render = insert_rule(pool, "render").await;
    let policy = insert_rule(pool, "selection_policy").await;
    let record_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO publish_records (
            idempotency_key, category_key, report_date, target_timezone,
            render_version, selection_policy_version, state, snapshot_frozen_at
        )
        VALUES (?, 'ai', '2026-04-28', 'Asia/Shanghai', ?, ?, 'snapshot_frozen', ?)
        RETURNING id
        "#,
    )
    .bind(format!(
        "ai-2026-04-28-v{}",
        TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
    .bind(render)
    .bind(policy)
    .bind(OffsetDateTime::now_utc())
    .fetch_one(pool)
    .await
    .unwrap();
    let (article_id, ai_result_id) = seed_article(pool).await;
    sqlx::query(
        r#"
        INSERT INTO publish_items (
            publish_record_id, position, article_id, article_ai_result_id,
            frozen_title, frozen_summary, frozen_tags_json, frozen_score,
            frozen_canonical_link, frozen_source_display_name
        )
        VALUES (?, 1, ?, ?, 'Title', 'summary', '["ai"]', 88, 'https://example.com/article', 'Source')
        "#,
    )
    .bind(record_id)
    .bind(article_id)
    .bind(ai_result_id)
    .execute(pool)
    .await
    .unwrap();
    record_id
}

async fn seed_article(pool: &SqlitePool) -> (i64, i64) {
    let config = insert_rule(pool, "config").await;
    let extractor = insert_rule(pool, "extractor").await;
    let prompt = insert_rule(pool, "prompt").await;
    let schema = insert_rule(pool, "ai_output_schema").await;
    let suffix = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let source_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, config_version) VALUES ('ai', ?, 'Source', ?, 'rss', ?) RETURNING id",
    )
    .bind(format!("source-{suffix}"))
    .bind(format!("https://example.com/{suffix}.xml"))
    .bind(config)
    .fetch_one(pool)
    .await
    .unwrap();
    let entry_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, title_raw, discovered_at, state, dedup_decision) VALUES (?, ?, ?, ?, 'title', ?, 'persisted', 'fresh') RETURNING id",
    )
    .bind(source_id)
    .bind(format!("uid-{suffix}"))
    .bind(format!("https://example.com/{suffix}"))
    .bind(format!("link-{suffix}"))
    .bind(OffsetDateTime::now_utc())
    .fetch_one(pool)
    .await
    .unwrap();
    let article_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO articles (content_hash, canonical_link, title, body_text, extractor_strategy, extractor_version, content_quality, word_count, origin_feed_entry_id, state) VALUES (?, 'https://example.com/article', 'Title', 'body', 'readability', ?, 'high', 1, ?, 'ready_for_publish') RETURNING id",
    )
    .bind(format!("hash-{suffix}"))
    .bind(extractor)
    .bind(entry_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let ai_result_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO article_ai_results (article_id, prompt_version, output_schema_version, model_id, state, summary, tags_json, importance_score, keep_decision) VALUES (?, ?, ?, 'model', 'succeeded', 'summary', '[\"ai\"]', 88, 1) RETURNING id",
    )
    .bind(article_id)
    .bind(prompt)
    .bind(schema)
    .fetch_one(pool)
    .await
    .unwrap();
    (article_id, ai_result_id)
}

async fn insert_rule(pool: &SqlitePool, kind: &str) -> i64 {
    let suffix = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) VALUES (?, ?, 'test rule', ?) RETURNING id",
    )
    .bind(kind)
    .bind(format!("{kind}-{suffix}"))
    .bind(format!("sha-{kind}-{suffix}"))
    .fetch_one(pool)
    .await
    .unwrap()
}

fn render_config() -> RenderConfig {
    RenderConfig {
        category_display_name: "AI".to_string(),
        report_title: "Daily AI".to_string(),
        generated_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        templates: RenderTemplates::default(),
    }
}
