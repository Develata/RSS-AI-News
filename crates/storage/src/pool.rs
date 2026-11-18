use std::{fmt, path::Path, str::FromStr, time::Duration};

use sqlx::{
    PgPool, SqlitePool,
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::StorageError;

/// 多方言存储池枚举。
///
/// W11-P4 起全部 repo 的 PG 路径均为真实实现（见
/// `docs-backup/design/storage-multi-dialect.md` §6.2 的 trait method `match`
/// 分发模式），不再存在 stub —— W17 已删除过渡期的 `require_sqlite` /
/// `PG_STUB_SUFFIX`（生产路径零调用）。
///
/// `Clone` 透传给底层 `SqlitePool` / `PgPool`（二者均为 `Arc` 包裹的 cheap-clone）。
/// `Debug` 手写脱敏 —— 不透传 sqlx 内部，避免未来日志意外暴露连接字符串。
#[derive(Clone)]
pub enum StoragePool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

impl StoragePool {
    /// 按 URL scheme 路由：支持 `postgres[ql]://`、`sqlite://`、`sqlite:` 和裸 SQLite 路径。
    ///
    /// `busy_timeout_ms` 仅对 SQLite 生效；PG 走 sqlx 默认的 connection 行为。
    pub async fn build(
        url: &str,
        max_connections: u32,
        busy_timeout_ms: u32,
    ) -> Result<Self, StorageError> {
        Self::validate_url_scheme(url)?;
        if Self::is_postgres_url(url) {
            let pool = build_pg_pool(url, max_connections).await?;
            return Ok(Self::Postgres(pool));
        }
        let sqlite_path_str = strip_sqlite_scheme(url);
        let sqlite_path = Path::new(sqlite_path_str);
        let pool = build_sqlite_pool(sqlite_path, max_connections, busy_timeout_ms).await?;
        Ok(Self::Sqlite(pool))
    }

    /// 构造查询专用 pool。SQLite 必须已存在且以 read-only 打开；PG 在每个
    /// connection 上设置 `default_transaction_read_only = on`。query CLI 固定单
    /// connection，避免一次性命令为只读 projection 建立不必要的连接池。
    pub async fn build_read_only(url: &str, busy_timeout_ms: u32) -> Result<Self, StorageError> {
        Self::validate_url_scheme(url)?;
        if Self::is_postgres_url(url) {
            let pool = build_pg_read_only_pool(url).await?;
            return Ok(Self::Postgres(pool));
        }
        let sqlite_path_str = strip_sqlite_scheme(url);
        let sqlite_path = Path::new(sqlite_path_str);
        let pool = build_sqlite_read_only_pool(sqlite_path, busy_timeout_ms).await?;
        Ok(Self::Sqlite(pool))
    }

    /// 不区分大小写 + 容忍前导空白：`POSTGRES://`、`  postgresql://` 都识别为 PG，
    /// 避免大小写绕过让 PG URL 落进 sqlite 路径（会被当成文件名打开，行为离谱）。
    pub fn is_postgres_url(url: &str) -> bool {
        url.trim_start()
            .split_once("://")
            .is_some_and(|(scheme, _)| {
                scheme.eq_ignore_ascii_case("postgres") || scheme.eq_ignore_ascii_case("postgresql")
            })
    }

    /// Validate only the supported scheme boundary; never include the URL in errors.
    /// Paths without `://` retain SQLite filename semantics.
    pub fn validate_url_scheme(url: &str) -> Result<(), StorageError> {
        if let Some((scheme, _)) = url.trim_start().split_once("://")
            && !scheme.eq_ignore_ascii_case("sqlite")
            && !scheme.eq_ignore_ascii_case("postgres")
            && !scheme.eq_ignore_ascii_case("postgresql")
        {
            return Err(StorageError::UnsupportedBackend(
                "expected sqlite, postgres, or postgresql URL scheme".to_owned(),
            ));
        }
        Ok(())
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

fn strip_sqlite_scheme(url: &str) -> &str {
    let trimmed = url.trim_start();
    for prefix in ["sqlite://", "sqlite:"] {
        if trimmed
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            return &trimmed[prefix.len()..];
        }
    }
    url
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

/// 打开已存在的 SQLite DB，且从连接层禁止写入。这里刻意不设置 WAL / synchronous：
/// 两者属于 writer-owned database PRAGMA，query-only CLI 不应在启动时改变持久状态。
pub async fn build_sqlite_read_only_pool(
    sqlite_path: &Path,
    busy_timeout_ms: u32,
) -> Result<SqlitePool, StorageError> {
    let options = SqliteConnectOptions::new()
        .filename(sqlite_path)
        .create_if_missing(false)
        .read_only(true)
        .busy_timeout(Duration::from_millis(u64::from(busy_timeout_ms)))
        .foreign_keys(true);

    SqlitePoolOptions::new()
        .max_connections(1)
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
    let options = pg_connect_options(url)?;
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(PG_ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
        .map_err(StorageError::from)
}

/// 两个 PG 建池入口共用的连接选项：先拒绝 `hostaddr`，再解析 URL。
///
/// CLI 在配置阶段已做同样的检查（exit 78）；这里是 storage 层自己的防线。
fn pg_connect_options(url: &str) -> Result<PgConnectOptions, StorageError> {
    if pg_hostaddr_in_use(url, pghostaddr_set()) {
        return Err(StorageError::Sqlx(sqlx::Error::Configuration(
            PG_HOSTADDR_UNSUPPORTED.into(),
        )));
    }
    PgConnectOptions::from_str(url).map_err(StorageError::from)
}

/// 用到 `hostaddr` 时的固定报错文本。不含任何环境值或 URL，避免带出凭据。
pub const PG_HOSTADDR_UNSUPPORTED: &str = "PGHOSTADDR and the hostaddr connection parameter \
    are not supported: SQLx 0.9 routes them differently from earlier releases and may reach \
    another server. Unset PGHOSTADDR and remove hostaddr from DATABASE_URL; put the server \
    address in the URL host";

/// 是否用到了 `hostaddr`：环境里有 `PGHOSTADDR`（任何值，包括空串），或 URL 带
/// `hostaddr` 参数。用到时一律拒绝连接。
///
/// SQLx 0.8 把 `hostaddr` 当作 host：`PGHOSTADDR` 会被 URL 里的 host 覆盖，空串也会
/// 顶替 `PGHOST`；URL 参数 `hostaddr` 走 TCP。SQLx 0.9 把它存为单独的字段，TCP 连接
/// 时优先用它，但 socket 路由（`PGHOST` 或 URL host 指向目录）又优先于它。这些组合下
/// 两版会连到不同的服务器，而 SQLx 0.9 没有清除该字段的接口，无法原样恢复旧行为。
/// 不涉及 `hostaddr` 时，两版的 host / socket 解析逐行相同。
///
/// 环境状态作为参数传入，测试因此不必修改进程环境（edition 2024 中 `set_var` 是
/// `unsafe`，并与并行测试互相干扰）。URL 无法解析时返回 `false`，由 SQLx 报格式错误。
pub fn pg_hostaddr_in_use(url: &str, pghostaddr_set: bool) -> bool {
    pghostaddr_set
        || url::Url::parse(url)
            .is_ok_and(|parsed| parsed.query_pairs().any(|(key, _)| key == "hostaddr"))
}

/// 进程环境里是否有 `PGHOSTADDR`，即 SQLx 读取的同一个环境。用 `var_os`，非 UTF-8 的值
/// 也算设置。
pub fn pghostaddr_set() -> bool {
    std::env::var_os("PGHOSTADDR").is_some()
}

async fn build_pg_read_only_pool(url: &str) -> Result<PgPool, StorageError> {
    let options = pg_connect_options(url)?;
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(PG_ACQUIRE_TIMEOUT)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_read_only = on")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
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

    const PG_URL: &str = "postgres://user:secret@db.example:5432/news";

    #[test]
    fn urls_without_hostaddr_are_allowed_when_pghostaddr_is_unset() {
        for url in [
            PG_URL,
            "postgres:///news",
            "postgres:///news?host=/var/run/postgresql",
            "postgres://user:secret@db.example/news?sslmode=require&host=other.example",
            "not a url",
        ] {
            assert!(!pg_hostaddr_in_use(url, false), "{url}");
        }
    }

    /// Any PGHOSTADDR (even empty) or URL hostaddr routes differently under
    /// SQLx 0.9 than under 0.8 for some PGHOST / URL host combination.
    #[test]
    fn pghostaddr_or_url_hostaddr_is_in_use() {
        for url in [PG_URL, "postgres:///news", "postgres:///news?host=/tmp"] {
            assert!(pg_hostaddr_in_use(url, true), "{url}");
        }
        for url in [
            format!("{PG_URL}?hostaddr=10.0.0.9"),
            format!("{PG_URL}?sslmode=require&hostaddr=10.0.0.9"),
            "postgres:///news?host=/tmp&hostaddr=10.0.0.9".to_string(),
            // query keys are percent-decoded, as SQLx decodes them
            format!("{PG_URL}?host%61ddr=10.0.0.9"),
        ] {
            assert!(pg_hostaddr_in_use(&url, false), "{url}");
        }
    }

    /// The builder refuses before any network I/O and never echoes the URL.
    #[tokio::test]
    async fn pg_pool_with_url_hostaddr_is_refused_without_leaking_the_url() {
        let error = build_pg_pool(&format!("{PG_URL}?hostaddr=10.0.0.9"), 1)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains(PG_HOSTADDR_UNSUPPORTED), "{message}");
        assert!(!message.contains("secret"), "{message}");
        assert!(!error.is_retryable());
    }

    #[test]
    fn sqlite_scheme_preserves_paths_and_supported_short_syntax() {
        for (input, expected) in [
            ("sqlite://data/news.db", "data/news.db"),
            ("sqlite:///tmp/news.db", "/tmp/news.db"),
            ("sqlite:data/news.db", "data/news.db"),
            ("sqlite::memory:", ":memory:"),
            (" SQLITE://data/news.db", "data/news.db"),
            ("data/news.db", "data/news.db"),
            ("  data/news.db", "  data/news.db"),
            ("新闻.db", "新闻.db"),
        ] {
            assert_eq!(strip_sqlite_scheme(input), expected);
            StoragePool::validate_url_scheme(input).unwrap();
        }
        for url in [
            "postgres://u@h/db",
            "postgresql://u@h/db",
            " POSTGRES://u@h/db",
        ] {
            StoragePool::validate_url_scheme(url).unwrap();
        }
    }

    #[tokio::test]
    async fn sqlite_memory_url_forms_open_without_creating_a_file() {
        for url in [
            ":memory:",
            "sqlite::memory:",
            "sqlite://:memory:",
            "SQLITE://:memory:",
        ] {
            let StoragePool::Sqlite(pool) = StoragePool::build(url, 1, 100).await.unwrap() else {
                panic!("SQLite URL must produce SQLite pool");
            };
            let file: String =
                sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(
                file.is_empty(),
                "memory URL must not create a database file"
            );
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn unsupported_database_schemes_fail_before_opening_storage() {
        for scheme in ["mysql", "http", "foo", "POSTGRE", "sqlité"] {
            let url = format!("{scheme}://private_user:private_password@host/db?token=secret");
            for error in [
                StoragePool::build(&url, 1, 100).await.unwrap_err(),
                StoragePool::build_read_only(&url, 100).await.unwrap_err(),
            ] {
                assert!(
                    matches!(error, StorageError::UnsupportedBackend(_)),
                    "{error:?}"
                );
                let diagnostic = format!("{error:?}");
                for secret in ["private_user", "private_password", "token=secret"] {
                    assert!(!diagnostic.contains(secret));
                }
            }
        }
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
}
