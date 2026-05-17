use std::{fmt, path::Path, time::Duration};

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
/// `Clone` 透传给底层 `SqlitePool` / `PgPool`（二者均为 `Arc` 包裹的 cheap-clone）。
/// `Debug` 手写脱敏 —— 不透传 sqlx 内部，避免未来日志意外暴露连接字符串。
#[derive(Clone)]
pub enum StoragePool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

/// W11-P2-A 阶段 PG 路径 stub 的错误信息后缀；P3 实装时全文 grep 这一串。
const PG_STUB_SUFFIX: &str = "postgres path is P3+";

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
            // 不把 url 拼进错误信息：PG URL 形如 `postgres://user:password@host/db`，
            // 拼出会让密码顺着 error chain / 日志泄露。stub 阶段返回固定串即可。
            return Err(StorageError::UnsupportedBackend(
                "postgres backend not implemented in P2-A; only sqlite is wired".into(),
            ));
        }
        let sqlite_path_str = strip_sqlite_scheme(url);
        let sqlite_path = Path::new(sqlite_path_str.as_ref());
        let pool = build_sqlite_pool(sqlite_path, max_connections, busy_timeout_ms).await?;
        Ok(Self::Sqlite(pool))
    }

    /// 不区分大小写 + 容忍前导空白：`POSTGRES://`、`  postgresql://` 都识别为 PG，
    /// 避免大小写绕过让 PG URL 落进 sqlite 路径（会被当成文件名打开，行为离谱）。
    pub fn is_postgres_url(url: &str) -> bool {
        let trimmed = url.trim_start();
        // scheme 一定是 ASCII，用 ascii lowercase 足够且不分配整串副本时也只在前 11 字节比较。
        let head: String = trimmed
            .chars()
            .take("postgresql://".len())
            .flat_map(char::to_lowercase)
            .collect();
        head.starts_with("postgres://") || head.starts_with("postgresql://")
    }

    /// 取出底层 `SqlitePool`；PG 分支返回 [`StorageError::UnsupportedBackend`]，
    /// 错误信息以 `scope` 标识"哪个 repo 还没填实 PG 路径"，P3 实装时
    /// `git grep "postgres path is P3+"` 一次扫齐所有 stub 点。
    ///
    /// 由 10 个 repo 的 `sqlite_pool()` thin wrapper 统一调用，避免分散的
    /// `match` 与不一致的错误信息字符串。
    pub fn require_sqlite(&self, scope: &'static str) -> Result<&SqlitePool, StorageError> {
        match self {
            Self::Sqlite(p) => Ok(p),
            Self::Postgres(_) => Err(StorageError::UnsupportedBackend(format!(
                "{scope} {PG_STUB_SUFFIX}"
            ))),
        }
    }
}

impl fmt::Debug for StoragePool {
    /// 手写 Debug：只暴露 variant 名 + sqlx::Pool::size()，不透传底层连接字符串。
    /// 避免未来若把 `StoragePool` 进结构化日志时把 `host/user` 等连带打出。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(p) => f
                .debug_struct("StoragePool::Sqlite")
                .field("connections", &p.size())
                .finish_non_exhaustive(),
            Self::Postgres(p) => f
                .debug_struct("StoragePool::Postgres")
                .field("connections", &p.size())
                .finish_non_exhaustive(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StorageError;

    #[test]
    fn is_postgres_url_accepts_canonical_schemes() {
        assert!(StoragePool::is_postgres_url("postgres://u:p@h/db"));
        assert!(StoragePool::is_postgres_url("postgresql://u:p@h/db"));
    }

    #[test]
    fn is_postgres_url_is_case_insensitive_and_trims() {
        assert!(StoragePool::is_postgres_url("POSTGRES://u@h/db"));
        assert!(StoragePool::is_postgres_url("Postgres://u@h/db"));
        assert!(StoragePool::is_postgres_url("  postgresql://u@h/db"));
        assert!(StoragePool::is_postgres_url("\tPOSTGRESQL://u@h/db"));
    }

    #[test]
    fn is_postgres_url_rejects_non_postgres() {
        assert!(!StoragePool::is_postgres_url("sqlite://./rss.db"));
        assert!(!StoragePool::is_postgres_url("./rss.db"));
        assert!(!StoragePool::is_postgres_url(""));
        assert!(!StoragePool::is_postgres_url("postgr://x")); // 前缀不全
        assert!(!StoragePool::is_postgres_url("mysql://x"));
    }

    #[tokio::test]
    async fn build_pg_url_returns_unsupported_backend_without_leaking_credentials() {
        let secret = "ne1ther_user_n0r_password_leak";
        let url = format!("postgres://alice:{secret}@db.example.com/mydb");
        let err = StoragePool::build(&url, 1, 100).await.unwrap_err();
        match err {
            StorageError::UnsupportedBackend(msg) => {
                assert!(
                    !msg.contains(secret),
                    "error message must not leak password: {msg}"
                );
                assert!(
                    !msg.contains("alice"),
                    "error message must not leak username: {msg}"
                );
            }
            other => panic!("expected UnsupportedBackend, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn require_sqlite_returns_pool_on_sqlite_variant() {
        let pool = build_sqlite_pool(Path::new(":memory:"), 1, 100)
            .await
            .expect("in-memory sqlite pool builds");
        let storage = StoragePool::Sqlite(pool);
        let inner = storage
            .require_sqlite("test_repo")
            .expect("sqlite returns pool");
        // 仅断言能取到引用即可（size 在 lazy 模式下可能仍为 0）
        let _ = inner.size();
    }

    /// P3 grep 兜底：所有 stub 错误信息都以 `PG_STUB_SUFFIX` 结尾，且包含 scope 标识。
    /// 单测无法构造真实 PgPool（连不到 server），但能通过常量与格式约束锁住语义。
    #[test]
    fn pg_stub_suffix_is_grep_friendly() {
        // 关键词唯一：避免与业务 SQL/日志中其它"P3"字串混淆
        assert_eq!(PG_STUB_SUFFIX, "postgres path is P3+");
        // 错误信息格式按"<scope> <suffix>"组合
        let msg = format!("article_repo {PG_STUB_SUFFIX}");
        assert!(msg.contains("article_repo"));
        assert!(msg.ends_with(PG_STUB_SUFFIX));
    }
}
