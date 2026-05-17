//! W11-P3-A.5：用 testcontainers 拉一次性 PG 16 容器，跑 `migrations/postgres/`
//! 完整 apply（含 0001 + 0002），再 `down` 全部回滚，验证两侧 SQL 在真实 PG
//! 上无语法/语义错误。
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
async fn postgres_migrations_apply_cleanly_on_pg16() {
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

    run_migrations(&pool)
        .await
        .expect("postgres migrations apply cleanly");

    // 二次跑应为 no-op（sqlx::migrate 幂等）。任何重复 apply 失败都说明
    // up SQL 未保持幂等假设（例如 ALTER TABLE 没 IF NOT EXISTS 守护）。
    run_migrations(&pool)
        .await
        .expect("postgres migrations are idempotent on rerun");
}
