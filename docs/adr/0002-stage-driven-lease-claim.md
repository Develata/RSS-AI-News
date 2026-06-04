# ADR 0002: 阶段驱动 + 租约领取（claim + lease）

- 日期：2026-02（建造期 W0–W1 期间确立）
- 状态：`accepted`
- 决策者：项目主作者

## Context

宪法 §3.5 要求"失败路径与可观测性内建于骨架"，[[ADR-0001]] 又规定 CLI single-shot、
不常驻、不共享进程内状态。这两条联立产生硬约束：

- **每段流水线必须可独立重启**：进程崩溃 / 容器被 OOM / cron 重叠触发都不能破坏数据
- **不能依赖进程内 mutex / 队列**：每次 run 是新进程，内存状态不延续
- **并发由数据库行级控制**：多 worker 可能同时跑 `ai-run`、`publish` 等

候选并发模型：
- (a) 单 worker + 文件锁（简单但放弃并发）
- (b) Redis / 外部队列（引入额外依赖，与 [[ADR-0001]] "少依赖"取向矛盾）
- (c) **DB 行级 claim + lease**：领任务时 `UPDATE ... WHERE state=Pending` 拿到行，
     写入 owner + lease_expires_at；过期由下一轮 reclaim 回收

## Decision

采用 **(c) DB 行级 claim + lease**：

- 所有跨进程的"任务 / 资源"通过数据库行表达，行上带 `state`、`owner`、`lease_started_at`、
  `lease_expires_at`、`attempt_count` 列
- 领取语义：`UPDATE ... SET state='Running', owner=?, lease_*=? WHERE state='Pending' AND ... RETURNING ...`
- 释放语义：`UPDATE ... WHERE owner=?` —— 用错 owner 静默失败（`rows_affected = 0`）
- 过期回收：周期性 `UPDATE ... SET state='Pending', owner=NULL WHERE lease_expires_at < now()`
- 4 类对象走此模型：`feed_entries` / `article_ai_results` / `publish_records` / `reindex_jobs`
- 状态机集中在 [`crates/domain/src/state.rs`](../../crates/domain/src/state.rs)

## Consequences

### 正面后果

- 多 worker 安全：行级 SQL 保证不会重复领取（`parallel_claim_returns_disjoint_rows` 测试覆盖）
- 崩溃容忍：worker 死亡 → lease 自然过期 → 下轮 reclaim 自动放回
- 单一真相源：所有任务状态在 DB，doctor / replay / 审计 / metrics 直查
- 调用方 Recover 不需要进程内 checkpoint：状态自描述

### 负面后果 / 代价

- 每段流水线必须显式建模为状态机：实现复杂度上升、文档负担增加
- SQL 写路径必须 cover claim / release / reclaim / 错主 / 终态等所有变迁，单 crate
  里 repo 函数数量较多（13 个 repo trait × N 个方法）
- 双方言（SQLite + PG）下行为对齐成本：partial unique、`UPDATE rows_affected` 语义等
  需要逐项验证（详见 [[ADR-0005]]）

### 后续行动

- 每个状态机的迁移图与不变量集中在 [../plan/08-state-machines.md](../plan/08-state-machines.md)
- claim/release/reclaim 在两个方言下行为对齐 → 测试覆盖见 [../acceptance-cases/pipelines/05-multi-dialect-storage.md](../acceptance-cases/pipelines/05-multi-dialect-storage.md)
- partial unique 用于"同 target 只有一个 active job"的 reindex 场景 → [[ADR-0004]]

## Links

- 设计：[../plan/05-storage.md](../plan/05-storage.md) §claim+lease，[../plan/08-state-machines.md](../plan/08-state-machines.md)
- 状态机定义：[`crates/domain/src/state.rs`](../../crates/domain/src/state.rs)
- 关键测试：`parallel_claim_returns_disjoint_rows` / `release_with_wrong_owner_returns_false` /
  `reclaim_expired_lease_clears_owner_and_allows_reclaim`（[`crates/storage/tests/concurrency_tests.rs`](../../crates/storage/tests/concurrency_tests.rs)）
- 相关 ADR：[[ADR-0001]] / [[ADR-0004]] / [[ADR-0005]]
