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
//!   （per-test 拉容器成本不可接受）。容器在测试进程退出时随 `OnceCell`
//!   静态生命周期一同 drop，PG 服务器随之停止 —— 残留 schema 自然消失。
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

/// 进程级 PG 容器单例。`OnceCell` 静态生命周期保证容器活到进程退出。
static PG_CONTAINER: OnceCell<ContainerAsync<Postgres>> = OnceCell::const_new();

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
        let drop_sql = format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sqlx::query(&drop_sql).execute(&self.admin),
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
                sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
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
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
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
    let container = PG_CONTAINER
        .get_or_init(|| async {
            Postgres::default()
                .with_tag("16-alpine")
                .start()
                .await
                .expect("start shared pg container")
        })
        .await;
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
