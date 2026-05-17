//! W11-P3-A.5 + P3-A-fix1.M1：用 testcontainers 拉一次性 PG 16 容器，跑
//! `migrations/postgres/` 完整 apply（含 0001 + 0002）+ 幂等 + 反向 undo 到 v0，
//! 验证两侧 SQL 在真实 PG 上无语法/语义错误。
//!
//! 默认 `#[ignore]`，需要 docker daemon。CI / 本地手跑用：
//!
//! ```
//! cargo test -p rss-ai-news-storage --test migrations_postgres_apply_tests -- --include-ignored
//! ```
//!
//! 走 [`rss_ai_news_storage::run_migrations`] —— 即 `match` 分发后的真实代码
//! 路径，避免"测试通过但生产路径漏了 Postgres 分支"。

use rss_ai_news_storage::{StoragePool, run_migrations};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ImageExt, runners::AsyncRunner},
};

#[tokio::test]
#[ignore = "需要 docker daemon；CI / 本地手跑加 --include-ignored"]
async fn postgres_migrations_apply_idempotent_and_undo_clean_on_pg16() {
    // postgres:16-alpine 镜像比 default 小（~80MB），冷启动 ~2s。
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("start pg container");

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container mapped port");

    // testcontainers-modules::postgres 默认 user=postgres, password=postgres, db=postgres
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let pool = StoragePool::build(&url, 4, 5_000)
        .await
        .expect("build pg pool");

    // ── 1. apply 0001 + 0002 ──
    run_migrations(&pool)
        .await
        .expect("postgres migrations apply cleanly");

    // ── 2. 二次跑应为 no-op（sqlx::migrate 幂等）──
    // 任何重复 apply 失败都说明 up SQL 未保持幂等假设（例如 ALTER TABLE
    // 没 IF NOT EXISTS 守护）。
    run_migrations(&pool)
        .await
        .expect("postgres migrations are idempotent on rerun");

    // ── 3. apply 后用户表必须存在 ──
    let pg_pool = match &pool {
        StoragePool::Postgres(p) => p,
        StoragePool::Sqlite(_) => panic!("expected pg pool in this test"),
    };
    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
    )
    .fetch_one(pg_pool)
    .await
    .expect("count tables after apply");
    // 0001+0002 共建 10 张用户表 + sqlx 自带 _sqlx_migrations
    assert_eq!(
        table_count, 11,
        "expected 10 user tables + _sqlx_migrations, got {table_count}"
    );

    // ── 4. undo 到 v0：反向 down 所有 migration ──
    // sqlx::Migrator::undo(executor, target) 会回滚所有 version > target 的
    // migration。target=0 表示反向到没有任何 migration 的状态。
    // 该路径会执行 0002.down.sql + 0001.down.sql，是 down.sql 在真实 PG 上的唯一
    // 测试入口（否则线上 rollback / down-grade 路径将完全无验证）。
    let migrator = sqlx::migrate!("../../migrations/postgres");
    migrator
        .undo(pg_pool, 0)
        .await
        .expect("postgres migrations undo cleanly to v0");

    // ── 5. undo 后用户表必须全部消失（只保留 _sqlx_migrations） ──
    let table_count_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
    )
    .fetch_one(pg_pool)
    .await
    .expect("count tables after undo");
    assert_eq!(
        table_count_after, 1,
        "after undo to v0 only _sqlx_migrations should remain, got {table_count_after}"
    );
}
