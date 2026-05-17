use std::{fmt, path::Path, str::FromStr, time::Duration};

use sqlx::{
    PgPool, SqlitePool,
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::StorageError;

/// 多方言存储池枚举。
///
/// W11-P3-A 起 PG 分支可真实构造 pool（参见 [`StoragePool::build`] / [`build_pg_pool`]），
/// 但 repo 业务方法 PG 路径仍是 stub —— [`StoragePool::require_sqlite`] 在 PG 分支
/// 仍返回 [`StorageError::UnsupportedBackend("<scope> postgres path is P3+")`]。
/// P3-C 起逐 repo 替换为真实 PG 实现，届时 require_sqlite 调用点逐个迁出。
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
    /// `busy_timeout_ms` 仅对 SQLite 生效；PG 走 sqlx 默认的 connection 行为。
    pub async fn build(
        url: &str,
        max_connections: u32,
        busy_timeout_ms: u32,
    ) -> Result<Self, StorageError> {
        if Self::is_postgres_url(url) {
            let pool = build_pg_pool(url, max_connections).await?;
            return Ok(Self::Postgres(pool));
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

/// W11-P3-A-fix1.L1：PG pool acquire 超时显式上限。
///
/// sqlx 的 `PgPoolOptions::acquire_timeout` 包含"新建连接 + 等待空闲连接"
/// 整段时间——CLI 启动期 / migration apply 时这一层就是 connect 上限。
///
/// 30 秒与 sqlx 默认一致：放宽到这个值是因为 testcontainers PG 容器冷启动
/// （含镜像 init / 初次 ready）有时接近 15-20s；本地测试与生产对超时的诉求
/// 不一致。生产 CLI 若想更快 fail，调用方再用 `tokio::time::timeout` 包一层
/// （[`build_pg_pool`] 内部不再做额外硬限）。
pub const PG_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// PG pool 构造：用 `PgConnectOptions::from_str` 解析 URL，避免直接把 url 字符串
/// 暴露在错误链里（sqlx::Error 本身不携带 url，但 connect 失败时的 message
/// 可能引用 host/port —— 密码不会泄露）。
///
/// 不调用 `connect_lazy_with`：W11-P3-A 出口标准要求"PG apply migration 成功"，
/// 这意味着 build 时即必须真实连通；连不上则立即 fail，调用方据此 fallback。
///
/// 超时上限：参见 [`PG_ACQUIRE_TIMEOUT`]。
pub async fn build_pg_pool(url: &str, max_connections: u32) -> Result<PgPool, StorageError> {
    // FromStr 实现解析 user:password@host:port/db 等 URL 字段，password 不进 Debug 输出。
    let options = PgConnectOptions::from_str(url).map_err(StorageError::from)?;
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(PG_ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
        .map_err(StorageError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// W11-P3-A：build 在 PG URL 上不再 stub，而是真实尝试连接。本测试用
    /// `127.0.0.1:1`（root-only port，普通进程必拒）验证 connect 失败时
    /// 错误链里不含用户名 / 密码字串 —— sqlx 的 connect error 仅引用 host:port。
    #[tokio::test]
    async fn build_pg_url_connect_error_does_not_leak_credentials() {
        let secret = "ne1ther_user_n0r_password_leak";
        // unique 用户名，避免与 host/port 字串巧合相同
        let user = "alice_qwert_xyzzy";
        let url = format!("postgres://{user}:{secret}@127.0.0.1:1/mydb");
        let err = StoragePool::build(&url, 1, 100).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            !msg.contains(secret),
            "error message must not leak password: {msg}"
        );
        assert!(
            !msg.contains(user),
            "error message must not leak username: {msg}"
        );
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

    /// W11-P3-A-fix1.L1：blackhole 地址（TEST-NET-1 `192.0.2.1`，RFC 5737 保留
    /// 不可路由）锁住 [`build_pg_pool`] 的 acquire 上限。`tokio::time::timeout`
    /// 加 5 秒 slack 兜底，超时即测试 fail（说明 [`PG_ACQUIRE_TIMEOUT`] 没生效）。
    ///
    /// `#[ignore]`：30+ 秒等待对默认 `cargo test` 太慢，与 docker apply 测试同等
    /// 走 `--include-ignored` 入口。
    #[tokio::test]
    #[ignore = "blackhole connect 测试 ~30s；--include-ignored 才跑"]
    async fn build_pg_pool_blackhole_respects_acquire_timeout() {
        // 192.0.2.0/24 是 RFC 5737 TEST-NET-1 documentation prefix，
        // 全球公网路由器都不会转发 —— TCP SYN 包必然走丢直到本地超时。
        let url = "postgres://u:p@192.0.2.1:5432/test";
        let slack = Duration::from_secs(5);
        let outcome = tokio::time::timeout(PG_ACQUIRE_TIMEOUT + slack, build_pg_pool(url, 1)).await;
        let inner = outcome.expect(
            "build_pg_pool must return within PG_ACQUIRE_TIMEOUT + slack; \
             outer timeout means acquire_timeout did not fire",
        );
        assert!(
            inner.is_err(),
            "expected connect error on blackhole, got Ok pool"
        );
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
