//! R2-B3: SQLite reports *extended* result codes through sqlx. A transient
//! lock error whose primary code is BUSY (5) or LOCKED (6) must be retryable
//! whatever its extended code (`517 SQLITE_BUSY_SNAPSHOT`,
//! `262 SQLITE_LOCKED_SHAREDCACHE`, …). The error here comes from the real
//! driver, not a mock.

mod common;

use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_storage::StorageError;
use sqlx::{Connection, SqlitePool};

/// WAL snapshot conflict: connection A reads in a deferred transaction,
/// connection B commits a write, then A tries to write. SQLite refuses A's
/// upgrade with `SQLITE_BUSY_SNAPSHOT` (517) immediately (no busy handler).
pub async fn busy_snapshot_error(pool: &SqlitePool) -> sqlx::Error {
    sqlx::query("CREATE TABLE IF NOT EXISTS b3_probe (id INTEGER PRIMARY KEY, v INTEGER)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO b3_probe (v) VALUES (0)")
        .execute(pool)
        .await
        .unwrap();
    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    let mut tx = a.begin().await.unwrap();
    let _: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM b3_probe")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO b3_probe (v) VALUES (1)")
        .execute(&mut *b)
        .await
        .unwrap();
    sqlx::query("UPDATE b3_probe SET v = 2")
        .execute(&mut *tx)
        .await
        .expect_err("the stale snapshot cannot be upgraded to a writer")
}

#[tokio::test]
async fn sqlite_extended_busy_code_from_the_driver_is_retryable() {
    let (_dir, pool) = common::make_test_pool_with_connections(2).await;
    let error = busy_snapshot_error(&pool).await;
    let code = match &error {
        sqlx::Error::Database(db) => db.code().map(|code| code.into_owned()),
        other => panic!("expected a database error, got {other:?}"),
    };
    assert_eq!(code.as_deref(), Some("517"), "SQLITE_BUSY_SNAPSHOT");

    let error = StorageError::from(error);
    assert!(error.is_retryable());
    assert!(ClassifiedError::is_retryable(&error));
}
