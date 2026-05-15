//! F15-4 W9-F3+F4：锁定 migration 0002 的 backfill 行为。
//!
//! 0001 时代 rule_versions 仅 UNIQUE(kind, version_tag)，允许同 kind 多行
//! （实际场景：`backfill ai` 写多个 prompt version、`publish --force`
//! 写多个 render tag）。0002 引入 partial unique index
//! uq_rule_versions_kind_active 限制同 kind 至多一行 active，因此必须在
//! 建立 index 前 backfill：
//!   - retired_at IS NOT NULL → status='superseded'
//!   - 同 kind 未退役多行时，仅 max(id) 留 active，其余 superseded

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rss_ai_news_storage::build_sqlite_pool;
use sqlx::SqlitePool;

const MIGRATION_0001_UP: &str = include_str!("../../../migrations/sqlite/0001_init.up.sql");
const MIGRATION_0002_UP: &str =
    include_str!("../../../migrations/sqlite/0002_reindex_jobs_and_rule_status.up.sql");

static TEST_DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

async fn build_pool_at_0001() -> (PathBuf, SqlitePool) {
    let counter = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rss-ai-news-storage-mig0002-{}-{nanos}-{counter}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    let db_path = dir.join("test.sqlite");
    let pool = build_sqlite_pool(&db_path, 1, 5_000)
        .await
        .expect("test pool should be created");
    sqlx::raw_sql(MIGRATION_0001_UP)
        .execute(&pool)
        .await
        .expect("0001 migration should apply");
    (dir, pool)
}

async fn apply_0002(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(MIGRATION_0002_UP).execute(pool).await?;
    Ok(())
}

async fn insert_0001_rule(pool: &SqlitePool, kind: &str, tag: &str, retired: bool) -> i64 {
    if retired {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, retired_at)
            VALUES (?, ?, 'desc', 'sha', '2024-01-01 00:00:00')
            RETURNING id
            "#,
        )
        .bind(kind)
        .bind(tag)
        .fetch_one(pool)
        .await
        .expect("insert retired rule")
    } else {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO rule_versions (kind, version_tag, description, payload_sha256)
            VALUES (?, ?, 'desc', 'sha')
            RETURNING id
            "#,
        )
        .bind(kind)
        .bind(tag)
        .fetch_one(pool)
        .await
        .expect("insert rule")
    }
}

#[tokio::test]
async fn backfill_keeps_single_kind_single_row_as_active() {
    let (_dir, pool) = build_pool_at_0001().await;
    let id = insert_0001_rule(&pool, "prompt", "v1", false).await;

    apply_0002(&pool).await.expect("0002 should apply");

    let status: String = sqlx::query_scalar("SELECT status FROM rule_versions WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("query status");
    assert_eq!(status, "active");
}

#[tokio::test]
async fn backfill_demotes_extra_rows_to_superseded_keeping_max_id_active() {
    let (_dir, pool) = build_pool_at_0001().await;
    let id_v1 = insert_0001_rule(&pool, "prompt", "v1", false).await;
    let id_v2 = insert_0001_rule(&pool, "prompt", "v2", false).await;
    let id_v3 = insert_0001_rule(&pool, "prompt", "v3", false).await;

    apply_0002(&pool).await.expect("0002 should apply");

    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, status FROM rule_versions WHERE kind = 'prompt' ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("query rows");

    assert_eq!(
        rows,
        vec![
            (id_v1, "superseded".to_string()),
            (id_v2, "superseded".to_string()),
            (id_v3, "active".to_string()),
        ]
    );
}

#[tokio::test]
async fn backfill_demotes_retired_rows_regardless_of_max_id() {
    let (_dir, pool) = build_pool_at_0001().await;
    // v1 未退役；v2 max(id) 但已退役 → v1 应保留 active，v2 → superseded
    let id_v1 = insert_0001_rule(&pool, "prompt", "v1", false).await;
    let id_v2_retired = insert_0001_rule(&pool, "prompt", "v2", true).await;

    apply_0002(&pool).await.expect("0002 should apply");

    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, status FROM rule_versions WHERE kind = 'prompt' ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("query rows");

    assert_eq!(
        rows,
        vec![
            (id_v1, "active".to_string()),
            (id_v2_retired, "superseded".to_string()),
        ]
    );
}

#[tokio::test]
async fn backfill_independently_handles_multiple_kinds() {
    let (_dir, pool) = build_pool_at_0001().await;
    let _prompt_v1 = insert_0001_rule(&pool, "prompt", "v1", false).await;
    let prompt_v2 = insert_0001_rule(&pool, "prompt", "v2", false).await;
    let render_v1 = insert_0001_rule(&pool, "render", "v1", false).await;

    apply_0002(&pool).await.expect("0002 should apply");

    let count_active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions WHERE status = 'active'")
            .fetch_one(&pool)
            .await
            .expect("count active");
    assert_eq!(count_active, 2);

    let active_prompt: i64 = sqlx::query_scalar(
        "SELECT id FROM rule_versions WHERE kind = 'prompt' AND status = 'active'",
    )
    .fetch_one(&pool)
    .await
    .expect("active prompt");
    assert_eq!(active_prompt, prompt_v2);

    let active_render: i64 = sqlx::query_scalar(
        "SELECT id FROM rule_versions WHERE kind = 'render' AND status = 'active'",
    )
    .fetch_one(&pool)
    .await
    .expect("active render");
    assert_eq!(active_render, render_v1);
}

#[tokio::test]
async fn partial_unique_index_holds_after_backfill() {
    let (_dir, pool) = build_pool_at_0001().await;
    insert_0001_rule(&pool, "prompt", "v1", false).await;
    insert_0001_rule(&pool, "prompt", "v2", false).await;

    apply_0002(&pool).await.expect("0002 should apply");

    // backfill 之后 v2 是 active；再写一行 active 同 kind 应被 partial unique 拒绝
    let result = sqlx::query(
        r#"
        INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status)
        VALUES ('prompt', 'v3', 'desc', 'sha', 'active')
        "#,
    )
    .execute(&pool)
    .await;
    assert!(
        result.is_err(),
        "partial unique index should reject second active row for kind='prompt'"
    );
}

#[tokio::test]
async fn fresh_db_with_no_existing_rows_applies_0002_cleanly() {
    let (_dir, pool) = build_pool_at_0001().await;
    apply_0002(&pool).await.expect("0002 should apply on empty");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 0);

    let reindex_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reindex_jobs")
        .fetch_one(&pool)
        .await
        .expect("count reindex_jobs");
    assert_eq!(reindex_count, 0);
}
