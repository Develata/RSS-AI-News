# ADR 0005: StoragePool 双方言 enum（SQLite + PostgreSQL）

- 日期：2026-05（F15 batch 期间确立）
- 状态：`accepted`
- 决策者：项目主作者

## Context

项目最初只支持 SQLite（local dev + 小规模生产足够）。F15 阶段 codex 二审指出：
`README.md` 与 `crates/config/src/env.rs` 都承诺 `DATABASE_URL` 切换 PostgreSQL，
但 `crates/storage/` 与 `crates/cli/src/context_factory.rs` 全部硬绑 `SqlitePool`。

需要在"实补 PG 支持"和"收缩 README 承诺"之间二选一。

候选实现路径（双方言情况下）：
- (a) `sqlx::Any` —— 使用 sqlx 的 Any backend，运行时分派 → 失去编译期类型检查、
     某些方言特性（如 partial index、TIMESTAMPTZ）表达不出来
- (b) **`StoragePool` enum** 显式分裂 + 每个 repo 内部 match 分派 → 类型安全但代码量大
- (c) 抽象成 trait + 两个泛型实现 —— 与 sqlx 的方言依赖语法不兼容（query! 必须知道具体方言）

## Decision

采用 **(b) `StoragePool` enum**：

```rust
pub enum StoragePool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}
```

- 每个 Repository trait 的实现持有 `StoragePool` 而非具体 pool
- 方法内部 `match` 分派到对应方言的 SQL
- 数据库 URL 通过 `is_postgres_url` 自动识别（前缀 `postgres://` / `postgresql://`）
- migrations 编号 + basename 一一对应：`migrations/sqlite/NNNN-name.sql` ↔ `migrations/postgres/NNNN-name.sql`
- migrate CLI 在两个方言下行为对齐：apply 全量、idempotent、可回滚

走 **方案 (ii) 实补 PG，不收缩 README**（详见 [[memory:project-postgres-dialect-decision]]）；
项目主作者于 2026-05-16 决议。

## Consequences

### 正面后果

- 编译期方言安全：每条 SQL 在 sqlx-cli 检查时知道目标方言
- 一份 README 承诺 ↔ 一份代码实现，契约/实现裂缝在 F15 期间一次性消除
- PG 拥有 SQLite 不具备的特性（TIMESTAMPTZ、partial index 语法略简洁、并发更好）
- 未来如需第三个方言（如 MySQL），可在 enum 上加变体而不影响已有路径

### 负面后果 / 代价

- 每个 repo 方法要写两份 SQL —— 代码量约 ×2
- migration 双份维护，CI 必须确保编号 / basename 对齐（`migrations_sqlite_and_postgres_have_matching_numbers_and_basenames`）
- 跨方言行为细节：`UPDATE rows_affected` 语义、binary 类型、partial unique 写法、占位符 `?` vs `$1`、
  时间函数等都要逐项映射（详见 [../plan/05-storage.md](../plan/05-storage.md) §多方言翻译规则）
- 测试矩阵翻倍：`dual_backend_smoke_tests.rs` 内每个 happy 都跑 sqlite + pg 两遍

### 后续行动

- enum 不再扩展第三方言（MySQL / D1 等）除非有明确生产需求
- `sqlx::Any` 路径明确**不**采用，避免类型安全损失（详见 [[memory:project-postgres-dialect-decision]]）
- CI `migrate` job 用 PG service container 端到端跑迁移
- PG 走"实补不收缩"路线 → [[ADR-0006]] 固化

## Links

- 设计：[../plan/05-storage.md](../plan/05-storage.md) §多方言
- 实现：[`crates/storage/src/pool.rs`](../../crates/storage/src/pool.rs)
- migration：[`migrations/sqlite/`](../../migrations/sqlite/)、[`migrations/postgres/`](../../migrations/postgres/)
- 验收：[../acceptance-cases/pipelines/05-multi-dialect-storage.md](../acceptance-cases/pipelines/05-multi-dialect-storage.md)
- 部署：[../plan/12-deployment.md](../plan/12-deployment.md) §PostgreSQL 切换
- 相关 ADR：[[ADR-0002]]、[[ADR-0004]]、[[ADR-0006]]
- 相关 memory：[[memory:project-postgres-dialect-decision]]
