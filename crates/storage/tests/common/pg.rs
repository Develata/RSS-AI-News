//! W11-P3-C-0：PostgreSQL 测试 fixture。
//!
//! 按 [`docs-backup/design/storage-multi-dialect.md`] §8.3 方案 1（URL 参数固化
//! `?options=-c%20search_path%3D...`）实现 per-test 独立 schema 隔离，
//! 避免 `SET search_path` 单次命中只影响连接池里某一条连接的"测试隔离假象"。
//!
//! ## 生命周期
//!
//! - **per-process 共享一个 PG 16-alpine 容器**：通过 [`PG_CONTAINER`]
//!   `OnceCell` 缓存；第一个测试拉起容器（冷启 ~2-5s），后续测试复用
//!   （per-test 拉容器成本不可接受）。Rust 不对 static 调用析构，容器的
//!   `Drop` 永远不会运行；由 [`spawn_container_reaper`] 拉起的 watcher 在测试
//!   进程退出后删除容器（连同匿名卷），残留 schema 随之消失。
//!
//! - **per-call admin pool**：每次 [`make_pg_test_pool`] 通过
//!   [`build_admin_pool`] fresh build 一个 `max=2` 的 admin pool（连
//!   默认 `public` schema），绑定到当前 `PgTestContext`；ctx drop 时
//!   PgPool 引用计数归零自动 close。W11-P4-fix4 之前是 `OnceCell` 全局
//!   共享，但跨 `#[tokio::test]` runtime drop 会 abort detached cleanup
//!   task 并漏 connection，CI `--test-threads=1` 上累积漏光 → 30s acquire
//!   超时；per-call 后漏只影响当前 ctx，不再污染后续测试。
//!
//! - **per-test 独立 schema + 独立 pool**：每个测试调
//!   [`make_pg_test_pool`] 拿一个 `PgTestContext`，内含：
//!   * 一个全新 schema `test_<pid>_<nanos>_<counter>`（与 sqlite fixture
//!     的命名同模式，三层叠加防碰撞）
//!   * 一个 `StoragePool::Postgres` pool，URL 嵌入 `search_path=<schema>`，
//!     此 pool 所有新连接自动落到该 schema
//!   * 全量 migration 已 apply 完毕（schema 内有 11 张表 + `_sqlx_migrations`）
//!   * `Drop` 时 detach 一个 `DROP SCHEMA ... CASCADE` task；失败静默
//!     （容器退出时整体清理兜底）
//!
//! ## 使用方式
//!
//! ```ignore
//! use crate::common::pg::make_pg_test_pool;
//!
//! #[tokio::test]
//! #[ignore = "需要 docker daemon；CI / 本地手跑加 --include-ignored"]
//! async fn my_pg_test() {
//!     let ctx = make_pg_test_pool().await;
//!     let pool = ctx.storage_pool();
//!     let repo = FeedSourceRepo::new_with_storage(pool.clone());
//!     // ... 测试主体
//! }
//! ```
//!
//! ## 为什么不用 sqlx::Migrator::undo + 共享 schema
//!
//! 共享 schema + per-test 清表会让并发测试串行化（清表期间互相阻塞），
//! 且 `_sqlx_migrations` 状态机难以原子回滚。per-test schema + apply
//! 全量虽然每条多 ~500ms，但并发友好且语义干净。

#![allow(dead_code)]

use std::{
    process::Child,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rss_ai_news_storage::{StoragePool, build_pg_pool, run_migrations};
use sqlx::PgPool;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use tokio::sync::OnceCell;

/// 进程级 PG 容器单例。`OnceCell` 静态生命周期保证容器活到进程退出；static 永不
/// drop，所以删除容器靠同一单例里的 reaper（见 [`spawn_container_reaper`]）。
static PG_CONTAINER: OnceCell<SharedPg> = OnceCell::const_new();

struct SharedPg {
    container: ContainerAsync<Postgres>,
    /// watcher 子进程。持有 `Child` 即持有它 stdin pipe 的写端；写端只在本进程
    /// 退出时由内核关闭，watcher 据此删除容器。
    _reaper: Option<Child>,
}

// W11-P4-fix4: 原 `PG_ADMIN_POOL: OnceCell<PgPool>` 跨 `#[tokio::test]` runtime
// 共享有 latent bug —— 每个 `#[tokio::test]` 默认 `current_thread` runtime；
// runtime drop 时会 abort `Drop::drop` 里 `handle.spawn` 出去的 detached
// `DROP SCHEMA ... CASCADE` task，被 abort 时连接没归还 admin pool；CI 串行
// `--test-threads=1` 跑 ~10+ 测试后 8 个 admin connection 漏光 → 后续测试
// CREATE SCHEMA 30s acquire 超时（W11-P4 首次 push 触发，本地 multi-thread
// 默认运行时不复现）。改 per-call：每次 `make_pg_test_pool` 自己 build
// admin pool(max=2) → CREATE SCHEMA，pool clone 存入 ctx；ctx drop 时
// PgPool 引用计数归零自动 close。漏 connection 只影响当前 ctx，不再
// 跨测试累积污染。

/// per-process schema 名 counter，与 PID + nanos 叠加；与 sqlite fixture 同模式。
static SCHEMA_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// PG 容器连接信息——`make_pg_test_pool` 时按 host/port 重建独立 URL。
#[derive(Debug, Clone)]
struct PgEndpoint {
    host: String,
    port: u16,
}

/// per-test 独立 schema + 独立 pool 的句柄。
///
/// `Drop` 时把 `DROP SCHEMA ... CASCADE` 通过 `tokio::spawn` detach 出去。
/// 即使 detach 失败（如运行时已停），残留 schema 也会随进程退出 / 容器停止
/// 清理；不抛错。
pub struct PgTestContext {
    pool: StoragePool,
    database_url: String,
    schema: String,
    /// 留 admin pool 引用——drop cleanup 时需要在 admin pool 上跑 DROP SCHEMA，
    /// 而不能用即将关闭的业务 pool。clone 仅是 Arc 增引用，开销可忽略。
    admin: PgPool,
    /// codex P3-C 评审 MEDIUM-1：是否已通过显式 [`Self::cleanup`] 清理；
    /// 已清的 context 在 [`Drop`] 里跳过 fire-and-forget 兜底，避免双重 DROP。
    cleaned: AtomicBool,
}

impl PgTestContext {
    /// 业务 pool（已嵌入 `search_path`，所有连接自动落在 `self.schema`）。
    pub fn storage_pool(&self) -> &StoragePool {
        &self.pool
    }

    /// 使用与业务 pool 相同 schema 构造 command-grade read-only pool。
    pub async fn read_only_storage_pool(&self) -> StoragePool {
        StoragePool::build_read_only(&self.database_url, 5_000)
            .await
            .expect("build per-test read-only pg pool")
    }

    /// 拿底层 `PgPool` 引用（断言 variant）；用于需要直接执行原生 SQL 的测试。
    pub fn pg_pool(&self) -> &PgPool {
        match &self.pool {
            StoragePool::Postgres(p) => p,
            StoragePool::Sqlite(_) => {
                panic!("PgTestContext should always hold Postgres variant")
            }
        }
    }

    /// 当前 schema 名（`test_<pid>_<nanos>_<counter>`）。
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// codex P3-C 评审 MEDIUM-1 修复 + P3-E-fix1 加固：显式 async cleanup。
    ///
    /// 顺序：
    ///   1. close 业务 pool（5s timeout）——释放 max=4 连接
    ///   2. admin pool 上 `DROP SCHEMA IF EXISTS ... CASCADE`（5s timeout）
    ///   3. 标记 `cleaned=true`，[`Drop`] 里跳过 fire-and-forget 兜底
    ///
    /// 推荐 P4 全量 PG rstest 测试在 happy 结束时显式 `ctx.cleanup().await`，
    /// 避免 ~80 个测试积累的 schema 等到进程结束才释放。
    /// **失败静默**：任一步出错（runtime 异常 / schema 已被外部清理 / 5s 超时）
    /// 都不抛——清理是 best-effort，容器停止会兜底回收整库。
    ///
    /// P3-E-fix1 教训：cargo test 默认并发执行多个 `#[tokio::test]`，多个
    /// cleanup 会同时打 admin pool（admin max 从 1 升 8 后并发空间够），
    /// 但若任一 `pool.close()` 或 `DROP SCHEMA` 卡 hung session，5s timeout
    /// 让最坏情况只损失 5s 而非测试卡死整套（曾见过 11000s 卡死）。
    pub async fn cleanup(&self) {
        if self.cleaned.swap(true, Ordering::SeqCst) {
            return; // 已清，幂等
        }
        // W11-P4-fix2.H2 lint：cleanup 是 best-effort，所有失败（超时 / DB
        // 错误）静默吞——容器停止会兜底回收整库。`.ok()` 显式表达忽略 Err。
        if let StoragePool::Postgres(p) = &self.pool {
            tokio::time::timeout(std::time::Duration::from_secs(5), p.close())
                .await
                .ok();
        }
        // AssertSqlSafe：DDL 标识符无法参数绑定；schema 名来自
        // next_schema_name()，字符集仅 [a-z0-9_]，不含外部输入。
        let drop_sql = format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sqlx::query(sqlx::AssertSqlSafe(drop_sql)).execute(&self.admin),
        )
        .await
        .ok();
    }
}

impl Drop for PgTestContext {
    fn drop(&mut self) {
        // 已通过显式 cleanup 清理：跳过兜底，避免双重 DROP / 无用 spawn。
        if *self.cleaned.get_mut() {
            return;
        }
        // 否则 fire-and-forget DROP SCHEMA 兜底——runtime 关闭可能让 task 来不及
        // 完成，容器停止时整库回收。
        let schema = std::mem::take(&mut self.schema);
        let admin = self.admin.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // W11-P4-fix2.H2 lint：detached cleanup 任务，错误无救赎；显式 .ok()。
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"
                )))
                .execute(&admin)
                .await
                .ok();
            });
        }
    }
}

/// 拉起（或复用）PG 容器，按 §8.3 方案 1 构造一个 per-test 独立 schema +
/// search_path 嵌入 URL 的 pool，全量 migration 已 apply。
///
/// 调用方应当：
///   - 在 `#[tokio::test]` 内调用（需要 tokio runtime）
///   - 标 `#[ignore]`（需要 docker daemon；CI 走 `--include-ignored`）
///
/// 不暴露错误：fixture 失败即测试失败，statically `expect` 简化测试主体。
pub async fn make_pg_test_pool() -> PgTestContext {
    let endpoint = pg_endpoint().await;
    let admin = build_admin_pool(&endpoint).await;

    let schema = next_schema_name();

    // 1) admin pool 上 CREATE SCHEMA。schema 名字符集仅 [a-z0-9_]，但仍引号
    //    包裹防御 PG 的 unquoted identifier 大小写折叠。
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA \"{schema}\"")))
        .execute(&admin)
        .await
        .expect("create per-test schema");

    // 2) URL 嵌入 search_path 选项——所有新连接自动落到该 schema。
    //    `options=-c key=value` 是 PG 标准连接参数（见 PG 文档
    //    "Other Defaults" §31.1.2.2）；URL 编码 ` ` 为 `%20`、`=` 为 `%3D`。
    let url = format!(
        "postgres://postgres:postgres@{host}:{port}/postgres?options=-c%20search_path%3D{schema}",
        host = endpoint.host,
        port = endpoint.port,
    );

    let pool = build_pg_pool(&url, 4)
        .await
        .expect("build per-test pg pool");
    let storage = StoragePool::Postgres(pool);

    // 3) 全量 migration apply 到该 schema。
    run_migrations(&storage)
        .await
        .expect("apply migrations to per-test schema");

    PgTestContext {
        pool: storage,
        database_url: url,
        schema,
        admin,
        cleaned: AtomicBool::new(false),
    }
}

/// 共享容器 + 拉端口信息。第一次调用拉镜像（postgres:16-alpine ~80MB）+
/// 启容器（~2-5s）；之后复用。
async fn pg_endpoint() -> PgEndpoint {
    let shared = PG_CONTAINER
        .get_or_init(|| async {
            let container = Postgres::default()
                .with_tag("16-alpine")
                .start()
                .await
                .expect("start shared pg container");
            let reaper = spawn_container_reaper(container.id());
            SharedPg {
                container,
                _reaper: reaper,
            }
        })
        .await;
    let container = &shared.container;
    let host = container
        .get_host()
        .await
        .expect("container host")
        .to_string();
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container mapped port");
    PgEndpoint { host, port }
}

/// 测试进程退出时删除共享容器：正常结束、测试失败、Ctrl-C、SIGKILL 都覆盖。
///
/// watcher 是一个 `sh` 子进程，阻塞读 stdin 直到 EOF，然后 `docker rm -f -v <id>`。
/// 选项与 testcontainers 自己删除容器时一致：强制，并删除镜像声明的匿名卷。
/// pipe 的写端放在永不 drop 的 static 里，所以只有本进程退出（任何方式）时内核才会
/// 关闭它。
///
/// 三处细节：
/// - watcher 在独立进程组里，终端 Ctrl-C 不会先杀掉它；
/// - stdout / stderr 接 null，cargo 和 acceptance runner 的输出捕获不必等它退出；
/// - 只按容器 id 删除，不按 label 删除，不会误删其他进程的容器。
///
/// 仅 Unix（Windows 上没有 `sh`）。拉不起 watcher 时只打印警告，容器会像以前一样残留，
/// 测试照常进行。
#[cfg(unix)]
fn spawn_container_reaper(container_id: &str) -> Option<Child> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let spawned = Command::new("sh")
        .args([
            "-c",
            r#"cat >/dev/null; docker rm -f -v "$1" >/dev/null 2>&1"#,
            "sh",
            container_id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    match spawned {
        Ok(child) => Some(child),
        Err(error) => {
            eprintln!(
                "warning: cannot start the PG container reaper ({error}); \
                 container {container_id} will outlive this test process"
            );
            None
        }
    }
}

#[cfg(not(unix))]
fn spawn_container_reaper(_container_id: &str) -> Option<Child> {
    None
}

/// admin pool（per-call）：连默认 `public` schema，仅给当前 `PgTestContext`
/// 跑 CREATE SCHEMA + 后续 cleanup 的 DROP SCHEMA 用。
///
/// W11-P4-fix4：原方案是 `OnceCell<PgPool>` 全局共享，跨多个
/// `#[tokio::test]`（每个一个 current_thread runtime）累积漏连接 →
/// CI `--test-threads=1` 上 admin pool acquire 30s 超时。改 per-call 后，
/// 每个 ctx 独占一个 admin pool（max=2，留 CREATE+DROP 各占 1 的余量），
/// ctx Drop 时 PgPool 引用计数归零自动 close —— 下一个测试新建 fresh pool，
/// 不会继承上一个 runtime 的连接残骸。
///
/// 代价：每个测试多 ~50-100ms admin pool build；CI 上 ~40 个测试 ~4s 总额。
async fn build_admin_pool(endpoint: &PgEndpoint) -> PgPool {
    let url = format!(
        "postgres://postgres:postgres@{host}:{port}/postgres",
        host = endpoint.host,
        port = endpoint.port,
    );
    build_pg_pool(&url, 2).await.expect("build admin pg pool")
}

fn next_schema_name() -> String {
    let counter = SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // schema 名长度 PG 上限 63 字节；`test_<u32>_<u128>_<usize>` 远低于上限。
    format!("test_{}_{nanos}_{counter}", std::process::id())
}
