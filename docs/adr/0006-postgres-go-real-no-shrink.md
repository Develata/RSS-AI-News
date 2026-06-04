# ADR 0006: PostgreSQL 走实补，不收缩 README 承诺

- 日期：2026-05-16
- 状态：`accepted`
- 决策者：项目主作者

## Context

F15 batch 推送完毕后，codex 二审 P0-2 finding 指出 `README.md` 与
`crates/config/src/env.rs:18-31` 都承诺通过 `DATABASE_URL` 可切换 PostgreSQL，但
`crates/storage/` 与 `crates/cli/src/context_factory.rs` 全部硬绑 `SqlitePool` ——
**契约与实现存在裂缝**。

两条收敛路径：
- (i) **收缩 README 承诺**：把"支持 PostgreSQL"从对外文档撤下，明确"当前仅 SQLite"
- (ii) **实补 PG**：让 `DATABASE_URL` 真正切换两方言 → 兑现 README 承诺

候选 (i) 短期成本低（只改文档），但：
- 长期欠债 —— 后续如要回到 PG，需要把同一议题重做一遍
- 与 [`docs-backup/design/storage-schema.md`](../../docs-backup/design/storage-schema.md)（旧建造期备份文档）
  早已声明的 "PostgreSQL 使用 TIMESTAMPTZ" 自相矛盾
- 用户视角："README 说支持 PG → 后续撤回"会留下信任损失

候选 (ii) 一次性消除裂缝。

## Decision

走 **方案 (ii) 实补 PG**。

具体路线：
- 4 周分阶段 P0–P4，每阶段独立可发布
- P0 产出 `docs/design/storage-multi-dialect.md`（旧 docs 设计契约，2026-05-16 落地）
- P1–P3 逐 crate 实补 PG path（storage / runtime / cli 三方向）
- P4 CI 加 PG service container

实施层面的关键决议：
- **enum 分裂**（不用 `sqlx::Any`）→ 见 [[ADR-0005]]
- **lease 表 `UPDATE...rows_affected` 谓词跨方言等价**，不引入 PG advisory_lock
- **schema 编号统一空间**：sqlite/postgres 同编号 → 同语义，保持迁移文件对齐（`migration_pair_parity_tests.rs`）
- PG 字段对齐 SQLite 字段：时间用 TIMESTAMPTZ，但**列名 / 表名 / 索引名一致**

## Consequences

### 正面后果

- README 与代码一致，消除外部信任损失
- 两方言长期并存，本地开发用 SQLite（零部署）、生产用 PG（高并发）
- 双方言测试矩阵反向倒逼 SQL 写法更"标准化"（避开方言专属语法）
- 与 [[ADR-0004]] 的 partial unique 语义在两方言上均成立

### 负面后果 / 代价

- 4 周工程投入（实际超出预估，详见 handoffs/）
- 双份测试（dual_backend_smoke 矩阵）+ 双份迁移文件
- 在不引入 advisory_lock 的前提下，PG 下的 reindex / claim 并发性能略低于"原生 PG 优化"路径
  —— 但优势是 SQLite path 等价语义

### 后续行动

- 与 [[ADR-0004]] 共同支撑后续 config-versioning 设计（多方言落地后 config-versioning 自动得到 PG 支持）
- 不引入 `sqlx::Any`；不引入第三方言（如 MySQL / D1）除非有明确生产需求
- 修订此决策的触发条件：PG 生产实践发现 enum 分裂的代码量代价超过收益（目前未达到）

## Links

- 设计契约（旧 docs 备份）：`docs-backup/design/storage-multi-dialect.md`
- 新设计章节：[../plan/05-storage.md](../plan/05-storage.md)
- 实现：[`crates/storage/src/pool.rs`](../../crates/storage/src/pool.rs)、[`migrations/postgres/`](../../migrations/postgres/)
- 部署：[../plan/12-deployment.md](../plan/12-deployment.md) §PostgreSQL 切换
- 验收：[../acceptance-cases/pipelines/05-multi-dialect-storage.md](../acceptance-cases/pipelines/05-multi-dialect-storage.md)
- 相关 memory：[[memory:project-postgres-dialect-decision]]
- 相关 ADR：[[ADR-0005]]
