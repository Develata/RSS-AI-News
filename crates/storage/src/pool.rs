use std::{path::Path, time::Duration};

use sqlx::{
    PgPool, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::StorageError;

/// 多方言存储池枚举。
///
/// W11-P2-A 期间 `Postgres` 分支仅作为类型占位 —— [`StoragePool::build`] 在收到
/// `postgres://` / `postgresql://` URL 时直接返回
/// [`StorageError::UnsupportedBackend`]，repo 内部 `match` 也统一 fail-fast。
/// 真实 PG pool 构造 + repo 业务方法 PG 路径自 P2-B / P3 起逐步接入。
///
/// `Clone` / `Debug` 透传给底层 `SqlitePool` / `PgPool`（二者均为 `Arc` 包裹的 cheap-clone）。
#[derive(Debug, Clone)]
pub enum StoragePool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

impl StoragePool {
    /// 按 URL scheme 路由：`postgres[ql]://` → Postgres，其余视为 SQLite 文件路径或 `sqlite://`。
    ///
    /// W11-P2-A 阶段 Postgres 路径直接返回 [`StorageError::UnsupportedBackend`]，
    /// P2-B 起接 `build_pg_pool`。
    pub async fn build(
        url: &str,
        max_connections: u32,
        busy_timeout_ms: u32,
    ) -> Result<Self, StorageError> {
        if Self::is_postgres_url(url) {
            return Err(StorageError::UnsupportedBackend(format!(
                "postgres backend (url={url}) not implemented in P2-A; only sqlite is wired"
            )));
        }
        let sqlite_path_str = strip_sqlite_scheme(url);
        let sqlite_path = Path::new(sqlite_path_str.as_ref());
        let pool = build_sqlite_pool(sqlite_path, max_connections, busy_timeout_ms).await?;
        Ok(Self::Sqlite(pool))
    }

    pub fn is_postgres_url(url: &str) -> bool {
        url.starts_with("postgres://") || url.starts_with("postgresql://")
    }
}

fn strip_sqlite_scheme(url: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = url.strip_prefix("sqlite://") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(url)
    }
}

pub async fn build_sqlite_pool(
    sqlite_path: &Path,
    max_connections: u32,
    busy_timeout_ms: u32,
) -> Result<SqlitePool, StorageError> {
    let options = SqliteConnectOptions::new()
        .filename(sqlite_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_millis(u64::from(busy_timeout_ms)))
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal);

    SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .map_err(StorageError::from)
}
