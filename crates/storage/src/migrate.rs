use sqlx::{FromRow, PgPool, SqlitePool};

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

/// codex P4 评审 MEDIUM-2 修复：返回当前 backend 内嵌 migration 集合的所有版本号
/// （从 `sqlx::migrate!` 宏编译期解析的 `migrations/sqlite|postgres` 目录），
/// 让 `cli migrate check` 能与 `_sqlx_migrations` 表对比检测 pending drift。
///
/// `sqlx::migrate!` 必须在 caller 处展开（宏路径相对 CARGO_MANIFEST_DIR），所以
/// 该 helper 必须留在 storage crate；cli 通过本函数获取版本号清单。
pub fn embedded_migration_versions(pool: &StoragePool) -> Vec<i64> {
    match pool {
        StoragePool::Sqlite(_) => sqlx::migrate!("../../migrations/sqlite")
            .iter()
            .map(|m| m.version)
            .collect(),
        StoragePool::Postgres(_) => sqlx::migrate!("../../migrations/postgres")
            .iter()
            .map(|m| m.version)
            .collect(),
    }
}

/// 只读取得已成功应用的 migration 版本。全新但已存在的空 DB 没有
/// `_sqlx_migrations`，按 0 applied 处理；其它错误原样返回。
pub async fn applied_migration_versions(pool: &StoragePool) -> Result<Vec<i64>, StorageError> {
    const QUERY: &str = "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version";
    let result = match pool {
        StoragePool::Sqlite(p) => sqlx::query_scalar::<_, i64>(QUERY).fetch_all(p).await,
        StoragePool::Postgres(p) => sqlx::query_scalar::<_, i64>(QUERY).fetch_all(p).await,
    };
    match result {
        Ok(versions) => Ok(versions),
        Err(sqlx::Error::Database(error))
            if error
                .message()
                .to_ascii_lowercase()
                .contains("_sqlx_migrations") =>
        {
            Ok(Vec::new())
        }
        Err(error) => Err(StorageError::from(error)),
    }
}

/// 计算代码内嵌但尚未成功应用的版本；纯内存、保持 embedded 顺序。
pub fn pending_migration_versions(pool: &StoragePool, applied: &[i64]) -> Vec<i64> {
    embedded_migration_versions(pool)
        .into_iter()
        .filter(|version| !applied.contains(version))
        .collect()
}

#[derive(Debug, FromRow)]
struct AppliedMigrationMetadata {
    version: i64,
    checksum: Vec<u8>,
    success: bool,
}

/// Fail closed unless the database migration history is exactly the embedded
/// history for the selected backend. Callers may perform the one-way pending
/// check first to preserve a more specific user-facing pending error.
pub async fn ensure_migration_state_exact(pool: &StoragePool) -> Result<(), StorageError> {
    const QUERY: &str = "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version";
    let applied = match pool {
        StoragePool::Sqlite(pool) => {
            sqlx::query_as::<_, AppliedMigrationMetadata>(QUERY)
                .fetch_all(pool)
                .await
        }
        StoragePool::Postgres(pool) => {
            sqlx::query_as::<_, AppliedMigrationMetadata>(QUERY)
                .fetch_all(pool)
                .await
        }
    }
    .map_err(StorageError::from)?;

    let embedded = match pool {
        StoragePool::Sqlite(_) => sqlx::migrate!("../../migrations/sqlite")
            .iter()
            .filter(|migration| migration.migration_type.is_up_migration())
            .map(|migration| (migration.version, migration.checksum.to_vec()))
            .collect::<Vec<_>>(),
        StoragePool::Postgres(_) => sqlx::migrate!("../../migrations/postgres")
            .iter()
            .filter(|migration| migration.migration_type.is_up_migration())
            .map(|migration| (migration.version, migration.checksum.to_vec()))
            .collect::<Vec<_>>(),
    };

    if applied.len() != embedded.len() {
        return Err(StorageError::Migration(format!(
            "migration history length mismatch: database={}, embedded={}",
            applied.len(),
            embedded.len()
        )));
    }

    for (row, (expected_version, expected_checksum)) in applied.iter().zip(embedded) {
        if !row.success {
            return Err(StorageError::Migration(format!(
                "migration {} is not marked successful",
                row.version
            )));
        }
        if row.version != expected_version {
            return Err(StorageError::Migration(format!(
                "migration version mismatch: database={}, embedded={expected_version}",
                row.version
            )));
        }
        if row.checksum.as_slice() != expected_checksum.as_slice() {
            return Err(StorageError::Migration(format!(
                "migration {} checksum mismatch",
                row.version
            )));
        }
    }

    Ok(())
}
