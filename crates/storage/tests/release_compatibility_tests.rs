mod common;

use std::borrow::Cow;

use rss_ai_news_storage::{StoragePool, ensure_migration_state_exact, run_migrations};
use sqlx::migrate::Migrator;

const LEGACY_ROWS: &str = include_str!("fixtures/v0.7.1/business.sql");
const SNAPSHOT: &str = "SELECT 'article', state || ':' || body_text FROM articles \
    UNION ALL SELECT 'ai', state || ':' || summary FROM article_ai_results \
    UNION ALL SELECT 'report', state || ':' || idempotency_key FROM publish_records \
    UNION ALL SELECT 'item', frozen_title || ':' || frozen_summary FROM publish_items";

// The existing immutability test pins every released migration byte. Filtering
// that set reproduces v0.7.1/v0.8.0 history without requiring Git tags in CI.
fn legacy(mut migrator: Migrator) -> Migrator {
    migrator.migrations = Cow::Owned(
        migrator
            .iter()
            .filter(|m| m.version <= 4)
            .cloned()
            .collect(),
    );
    migrator
}

async fn snapshot(pool: &StoragePool) -> Vec<(String, String)> {
    match pool {
        StoragePool::Sqlite(pool) => sqlx::query_as(SNAPSHOT).fetch_all(pool).await.unwrap(),
        StoragePool::Postgres(pool) => sqlx::query_as(SNAPSHOT).fetch_all(pool).await.unwrap(),
    }
}

async fn assert_upgrade_preserves_rows(pool: &StoragePool) {
    assert!(ensure_migration_state_exact(pool).await.is_err());
    match pool {
        StoragePool::Sqlite(pool) => {
            sqlx::raw_sql(LEGACY_ROWS).execute(pool).await.unwrap();
        }
        StoragePool::Postgres(pool) => {
            sqlx::raw_sql(LEGACY_ROWS).execute(pool).await.unwrap();
        }
    }
    let before = snapshot(pool).await;
    assert_eq!(before.len(), 4);
    assert!(before.contains(&(
        "item".into(),
        "Frozen legacy title:Frozen legacy summary".into()
    )));
    run_migrations(pool).await.unwrap();
    ensure_migration_state_exact(pool).await.unwrap();
    assert_eq!(snapshot(pool).await, before);
    run_migrations(pool).await.unwrap();
    assert_eq!(snapshot(pool).await, before);
}

#[tokio::test]
async fn sqlite_v0_7_1_database_upgrades_and_index_migration_rolls_back_without_data_changes() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let old = legacy(sqlx::migrate!("../../migrations/sqlite"));
    old.run(&pool).await.unwrap();
    let storage = StoragePool::Sqlite(pool.clone());
    assert_upgrade_preserves_rows(&storage).await;
    let before = snapshot(&storage).await;
    sqlx::migrate!("../../migrations/sqlite")
        .undo(&pool, 4)
        .await
        .unwrap();
    old.run(&pool).await.unwrap();
    assert_eq!(snapshot(&storage).await, before);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires Docker PostgreSQL fixture"]
async fn pg_v0_7_1_database_upgrades_and_index_migration_rolls_back_without_data_changes() {
    let ctx = common::pg::make_pg_test_pool().await;
    let current = sqlx::migrate!("../../migrations/postgres");
    current.undo(ctx.pg_pool(), 4).await.unwrap();
    let old = legacy(sqlx::migrate!("../../migrations/postgres"));
    old.run(ctx.pg_pool()).await.unwrap();
    assert_upgrade_preserves_rows(ctx.storage_pool()).await;
    let before = snapshot(ctx.storage_pool()).await;
    current.undo(ctx.pg_pool(), 4).await.unwrap();
    old.run(ctx.pg_pool()).await.unwrap();
    assert_eq!(snapshot(ctx.storage_pool()).await, before);
    ctx.cleanup().await;
}
