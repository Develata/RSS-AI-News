use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rss_ai_news_runtime::doctor::deep_scan::{InvariantId, run};
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use sqlx::SqlitePool;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

macro_rules! happy_path_test {
    ($name:ident, $id:expr) => {
        #[tokio::test]
        async fn $name() {
            let pool = make_pool().await;
            let report = run(&pool).await.expect("deep scan");
            assert_eq!(violations(&report, $id), 0);
        }
    };
}

happy_path_test!(i1_happy_path, InvariantId::I1);
happy_path_test!(i2_happy_path, InvariantId::I2);
happy_path_test!(i3_happy_path, InvariantId::I3);
happy_path_test!(i4_happy_path, InvariantId::I4);
happy_path_test!(i4a_prime_happy_path, InvariantId::I4APrime);
happy_path_test!(i4b_prime_happy_path, InvariantId::I4BPrime);
happy_path_test!(i5_happy_path, InvariantId::I5);
happy_path_test!(i6_happy_path, InvariantId::I6);
happy_path_test!(i8_happy_path, InvariantId::I8);

#[tokio::test]
async fn i4_violation_ready_for_publish_with_non_keep_ai_row() {
    let pool = make_pool().await;
    let article_id = insert_article(&pool, "ready_for_publish").await;
    insert_ai_result(&pool, article_id, "filtered", None, None).await;

    let report = run(&pool).await.expect("deep scan");

    assert_eq!(violations(&report, InvariantId::I4), 1);
}

#[tokio::test]
async fn i4a_prime_violation_publish_item_bound_to_non_keep_ai_result() {
    let pool = make_pool().await;
    let article_id = insert_article(&pool, "ready_for_publish").await;
    let ai_result_id = insert_ai_result(&pool, article_id, "succeeded", Some(0), None).await;
    let publish_record_id = insert_publish_record(&pool, "snapshot_frozen").await;
    insert_publish_item(&pool, publish_record_id, article_id, Some(ai_result_id)).await;

    let report = run(&pool).await.expect("deep scan");

    assert_eq!(violations(&report, InvariantId::I4APrime), 1);
}

#[tokio::test]
async fn i4b_prime_violation_passthrough_publish_item_with_ai_row() {
    let pool = make_pool().await;
    let article_id = insert_article(&pool, "ready_for_publish").await;
    insert_ai_result(&pool, article_id, "succeeded", Some(1), None).await;
    let publish_record_id = insert_publish_record(&pool, "snapshot_frozen").await;
    insert_publish_item(&pool, publish_record_id, article_id, None).await;

    let report = run(&pool).await.expect("deep scan");

    assert_eq!(violations(&report, InvariantId::I4BPrime), 1);
}

#[tokio::test]
async fn i6_violation_successful_publish_record_with_unpublished_article() {
    let pool = make_pool().await;
    let article_id = insert_article(&pool, "ready_for_publish").await;
    let ai_result_id = insert_ai_result(&pool, article_id, "succeeded", Some(1), None).await;
    let publish_record_id = insert_publish_record(&pool, "published_remote").await;
    insert_publish_item(&pool, publish_record_id, article_id, Some(ai_result_id)).await;

    let report = run(&pool).await.expect("deep scan");

    assert_eq!(violations(&report, InvariantId::I6), 1);
}

async fn make_pool() -> SqlitePool {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    // F8-3 W4-3: 加纳秒抗 PID 跨进程复用 + 残留文件碰撞。
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rss-ai-news-runtime-doctor-{}-{nanos}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let db_path = dir.join("test.sqlite");
    let pool = build_sqlite_pool(&db_path, 1, 5_000).await.expect("pool");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrations");
    pool
}

fn violations(
    report: &rss_ai_news_runtime::doctor::deep_scan::DeepScanReport,
    id: InvariantId,
) -> u64 {
    report
        .results
        .iter()
        .find(|result| result.id == id)
        .expect("invariant result")
        .total_count
}

async fn insert_rule(pool: &SqlitePool, kind: &str) -> i64 {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) VALUES (?, ?, 'test', ?) RETURNING id",
    )
    .bind(kind)
    .bind(format!("{kind}-{id}"))
    .bind(format!("sha-{id}"))
    .fetch_one(pool)
    .await
    .expect("rule")
}

async fn insert_source(pool: &SqlitePool) -> i64 {
    let config_version = insert_rule(pool, "config").await;
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, config_version)
        VALUES ('ai', ?, 'Source', ?, 'rss', ?)
        RETURNING id
        "#,
    )
    .bind(format!("source-{id}"))
    .bind(format!("https://example.com/{id}.xml"))
    .bind(config_version)
    .fetch_one(pool)
    .await
    .expect("source")
}

async fn insert_feed_entry(pool: &SqlitePool) -> i64 {
    let source_id = insert_source(pool).await;
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw, discovered_at, state, dedup_decision
        )
        VALUES (?, ?, ?, ?, 'title', datetime('now'), 'persisted', 'fresh')
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(format!("uid-{id}"))
    .bind(format!("https://example.com/{id}"))
    .bind(format!("hash-{id}"))
    .fetch_one(pool)
    .await
    .expect("feed entry")
}

async fn insert_article(pool: &SqlitePool, state: &str) -> i64 {
    let extractor_version = insert_rule(pool, "extractor").await;
    let entry_id = insert_feed_entry(pool).await;
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let article_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO articles (
            content_hash, canonical_link, title, body_text, extractor_strategy,
            extractor_version, content_quality, word_count, origin_feed_entry_id, state
        )
        VALUES (?, ?, 'title', 'body', 'readability', ?, 'high', 1, ?, ?)
        RETURNING id
        "#,
    )
    .bind(format!("content-{id}"))
    .bind(format!("https://example.com/article/{id}"))
    .bind(extractor_version)
    .bind(entry_id)
    .bind(state)
    .fetch_one(pool)
    .await
    .expect("article");
    sqlx::query("UPDATE feed_entries SET article_id = ? WHERE id = ?")
        .bind(article_id)
        .bind(entry_id)
        .execute(pool)
        .await
        .expect("link feed entry");
    article_id
}

async fn insert_ai_result(
    pool: &SqlitePool,
    article_id: i64,
    state: &str,
    keep_decision: Option<i32>,
    lease_expires_at: Option<&str>,
) -> i64 {
    let prompt_version = insert_rule(pool, "prompt").await;
    let output_schema_version = insert_rule(pool, "ai_output_schema").await;
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO article_ai_results (
            article_id, prompt_version, output_schema_version, model_id, state,
            keep_decision, lease_expires_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(article_id)
    .bind(prompt_version)
    .bind(output_schema_version)
    .bind(format!("model-{id}"))
    .bind(state)
    .bind(keep_decision)
    .bind(lease_expires_at)
    .fetch_one(pool)
    .await
    .expect("ai result")
}

async fn insert_publish_record(pool: &SqlitePool, state: &str) -> i64 {
    let render_version = insert_rule(pool, "render").await;
    let policy_version = insert_rule(pool, "selection_policy").await;
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO publish_records (
            idempotency_key, category_key, report_date, target_timezone,
            render_version, selection_policy_version, state
        )
        VALUES (?, 'ai', '2026-04-30', 'Asia/Shanghai', ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(format!("publish-{id}"))
    .bind(render_version)
    .bind(policy_version)
    .bind(state)
    .fetch_one(pool)
    .await
    .expect("publish record")
}

async fn insert_publish_item(
    pool: &SqlitePool,
    publish_record_id: i64,
    article_id: i64,
    article_ai_result_id: Option<i64>,
) -> i64 {
    let frozen_score = article_ai_result_id.map(|_| 80_i32);
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO publish_items (
            publish_record_id, position, article_id, article_ai_result_id,
            frozen_title, frozen_summary, frozen_tags_json, frozen_score,
            frozen_canonical_link, frozen_source_display_name
        )
        VALUES (?, 1, ?, ?, 'title', 'summary', '[]', ?, 'https://example.com/article', 'Source')
        RETURNING id
        "#,
    )
    .bind(publish_record_id)
    .bind(article_id)
    .bind(article_ai_result_id)
    .bind(frozen_score)
    .fetch_one(pool)
    .await
    .expect("publish item")
}
