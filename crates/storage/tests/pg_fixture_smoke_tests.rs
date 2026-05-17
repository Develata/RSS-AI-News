//! W11-P3-C-0 smoke：验证 [`crate::common::pg::make_pg_test_pool`] fixture 本身
//! 在真实 PG 16-alpine 容器上能：
//!   1. 启容器（首次）/ 复用容器（二次）
//!   2. CREATE 独立 schema
//!   3. 在该 schema 内 apply 全量 migration（10 张用户表 + `_sqlx_migrations`）
//!   4. 两个并发 `PgTestContext` 互相不可见对方建的表
//!
//! 这条 smoke 是 P3-C-1..P3-C-4 所有 PG-only 测试的前置依赖。fixture 一旦
//! 回归，这里先 fail，避免业务测试一头雾水。
//!
//! 默认 `#[ignore]`，需要 docker。

mod common;

use common::pg::make_pg_test_pool;
use sqlx::Row;

#[tokio::test]
#[ignore = "需要 docker daemon；CI / 本地手跑加 --include-ignored"]
async fn pg_fixture_creates_isolated_schema_with_full_migrations() {
    let ctx = make_pg_test_pool().await;
    let schema = ctx.schema().to_string();
    let pool = ctx.pg_pool();

    // 1) 当前连接的 search_path 必须包含本测试的 schema
    //    （URL `options=-c search_path=test_xxx` 生效证据）
    let search_path: String = sqlx::query_scalar("SHOW search_path")
        .fetch_one(pool)
        .await
        .expect("SHOW search_path on per-test pool");
    assert!(
        search_path.contains(&schema),
        "search_path={search_path:?} should include schema={schema:?}"
    );

    // 2) 该 schema 下用户表数 = 10（0001+0002 共建），加 `_sqlx_migrations` = 11
    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM information_schema.tables \
         WHERE table_schema = $1 AND table_type = 'BASE TABLE'",
    )
    .bind(&schema)
    .fetch_one(pool)
    .await
    .expect("count tables in per-test schema");
    assert_eq!(
        table_count, 11,
        "per-test schema should contain 10 user tables + _sqlx_migrations, got {table_count}"
    );

    // 3) `public` schema 必须为空（migration 没污染默认 schema）
    let public_user_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
    )
    .fetch_one(pool)
    .await
    .expect("count tables in public schema");
    assert_eq!(
        public_user_tables, 0,
        "public schema should remain empty when search_path routes migrations to per-test schema"
    );
}

#[tokio::test]
#[ignore = "需要 docker daemon；CI / 本地手跑加 --include-ignored"]
async fn pg_fixture_two_contexts_are_isolated() {
    let ctx_a = make_pg_test_pool().await;
    let ctx_b = make_pg_test_pool().await;
    assert_ne!(
        ctx_a.schema(),
        ctx_b.schema(),
        "two fixture contexts must use distinct schemas"
    );

    // 在 A 里 INSERT 一条 rule_versions，B 里应该读不到（不同 schema）
    sqlx::query(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ($1, $2, $3, $4, 'pending')",
    )
    .bind("config")
    .bind("isolation-probe")
    .bind("probe")
    .bind("sha-probe")
    .execute(ctx_a.pg_pool())
    .await
    .expect("INSERT into ctx_a.rule_versions");

    let rows_in_b = sqlx::query("SELECT id FROM rule_versions WHERE version_tag = $1")
        .bind("isolation-probe")
        .fetch_all(ctx_b.pg_pool())
        .await
        .expect("SELECT from ctx_b.rule_versions");
    assert!(
        rows_in_b.is_empty(),
        "ctx_b must not see ctx_a's row (got {} row(s))",
        rows_in_b.len()
    );

    // A 里能看到自己写的行（sanity）
    let rows_in_a = sqlx::query("SELECT id FROM rule_versions WHERE version_tag = $1")
        .bind("isolation-probe")
        .fetch_all(ctx_a.pg_pool())
        .await
        .expect("SELECT from ctx_a.rule_versions");
    assert_eq!(rows_in_a.len(), 1, "ctx_a should see exactly its own row");
    // 静默 unused
    let _ = rows_in_a[0].columns();
}
