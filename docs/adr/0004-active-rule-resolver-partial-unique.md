# ADR 0004: active_rule resolver + partial unique index

- 日期：2026-03（W3 引入；W10 收口；F15-fix6 局部完善）
- 状态：`accepted`（伴随一个公开"已知缺口" — 详见后续行动）
- 决策者：项目主作者

## Context

规则的语义随版本演进（link_hash 算法、content 规范化、分类规则、prompt 模板等）。
为了在升级时**不破坏已读取的历史数据**，每条规则被刻画成一个版本化对象：

- 表：`rule_versions(kind, version_tag, payload_sha256, status, ...)`
- `status ∈ {pending, active, superseded, retired}`
- **同一 `kind` 在任一时刻只能有 0 或 1 个 active 行**

需要解决：
1. 读路径如何快速查到当前 active 规则？
2. reindex 升级新版本时如何原子切换（旧 active → superseded、新 pending → active）？
3. 并发场景下如何防止"同 target 多个 active job 进行 reindex"？

候选方案：
- (a) 业务层每次扫表 + 进程锁 → 与 single-shot 冲突
- (b) **partial unique index** `UNIQUE (kind) WHERE status='active'` + active resolver 函数 + 事务内切换
- (c) 引入"current rule"单独表，rule_versions 仅作历史 → 双表一致性更复杂

## Decision

采用 **(b) partial unique + active resolver + 事务切换**：

- `rule_versions` 表上建偏唯一索引：`CREATE UNIQUE INDEX ... ON rule_versions(kind) WHERE status='active'`
- 读路径：`active_rule(kind)` 函数（同时为 SQLite 与 PG 提供）
- reindex 升级流程在**同一事务**内完成：旧 active → superseded、新 pending → active
- reindex_jobs 同 target 唯一性：`UNIQUE (target) WHERE state IN ('pending', 'running')`，
  防止并发的两个 worker 同时 start 相同 target 的 reindex
- migration 0002 backfill：对存量数据，把同 kind 多余的 active 行降级为 superseded，
  保留 `MAX(id)`；retired 行无论 id 都 demote（详见 `migration_0002_backfill_tests.rs`）

## Consequences

### 正面后果

- 读路径只查 active 行（partial index 上常数级 lookup）
- 切换是事务内原子操作 —— reindex 中途崩溃不会让 active 双开
- mark_failed 时保留新 pending + 不降旧 active —— 失败后读路径仍能拿到旧 active
- bootstrap 兜底：首次启动空 DB 时也能写入"ingest-bootstrap" placeholder 规则，让数据库引用完整性不被破坏

### 负面后果 / 代价

- SQLite 与 PG partial unique 语法不同 —— 双方言要分别建索引
- bootstrap placeholder 滞留有"已知缺口"（见后续行动）
- "concurrent rule version seed" 在 PG 下偶发需要 retry（`pg_rule_version_concurrent_seed_no_partial_unique_failure` 覆盖）

### 后续行动

**已知缺口**（公开标记，不阻塞 v0.3.0）：bootstrap placeholder rule（`payload_sha256='ingest-bootstrap'`）
是某 SQLite 实例上 `kind='config'` 的首个 active 行，之后用户启动真实 config 时
`get_or_create_config_version_async` 走 `INSERT ... ON CONFLICT DO NOTHING RETURNING id` —
**不会自动 demote 旧 placeholder、promote 真实 config**。

候选收敛路径（待 W10 之后或独立 `docs/design/config-versioning.md` 内规划）：
- (a) admin `rule-version promote-config` 子命令手动切换
- (b) 真实 config seed 时检测 placeholder active 自动 demote + promote
- (c) doctor 检测 active config payload_sha256='ingest-bootstrap' 时 warn 提示

详见 [[memory:project-bootstrap-rule-active-seam]] 与 [../plan/06-config.md](../plan/06-config.md) §11 末尾标注。

## Links

- 设计：[../plan/05-storage.md](../plan/05-storage.md) §rule_versions / §reindex
- 验收：[../acceptance-cases/commands/reindex.md](../acceptance-cases/commands/reindex.md)、[../acceptance-cases/commands/migrate.md](../acceptance-cases/commands/migrate.md)
- 实现：[`crates/storage/src/repo/rule_version.rs`](../../crates/storage/src/repo/rule_version.rs)、[`crates/storage/src/repo/reindex_job.rs`](../../crates/storage/src/repo/reindex_job.rs)
- 迁移：[`migrations/sqlite/0002_reindex_jobs_and_rule_status.{up,down}.sql`](../../migrations/sqlite/)、[`migrations/postgres/0002_reindex_jobs_and_rule_status.{up,down}.sql`](../../migrations/postgres/)
- 关键测试：`partial_unique_index_holds_after_backfill`、`reindex_promotes_rule_version_to_active_on_completion`、`reindex_mark_failed_keeps_old_active_and_pending_new_rule_version`
- 相关 ADR：[[ADR-0002]]、[[ADR-0005]]
