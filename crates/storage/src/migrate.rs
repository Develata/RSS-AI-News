use sqlx::{PgPool, SqlitePool};

use crate::{StorageError, StoragePool};

/// 按 backend 分发到对应方言目录的 migration。
///
/// W11-P3-A：迁出旧的 `&SqlitePool` 签名，统一接 `&StoragePool`，
/// 两侧 migration 文件路径分别相对 `crates/storage/`：
///   - SQLite: `../../migrations/sqlite`
///   - Postgres: `../../migrations/postgres`
///
/// `sqlx::migrate!` 的路径解析基准是 `CARGO_MANIFEST_DIR`（调用宏的 crate 根）。
pub async fn run_migrations(pool: &StoragePool) -> Result<(), StorageError> {
    match pool {
        StoragePool::Sqlite(p) => run_sqlite_migrations(p).await,
        StoragePool::Postgres(p) => run_postgres_migrations(p).await,
    }
}

async fn run_sqlite_migrations(pool: &SqlitePool) -> Result<(), StorageError> {
    sqlx::migrate!("../../migrations/sqlite")
        .run(pool)
        .await
        .map_err(|error| StorageError::Migration(error.to_string()))
}

async fn run_postgres_migrations(pool: &PgPool) -> Result<(), StorageError> {
    sqlx::migrate!("../../migrations/postgres")
        .run(pool)
        .await
        .map_err(|error| StorageError::Migration(error.to_string()))
}
