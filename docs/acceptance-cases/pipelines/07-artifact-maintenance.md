# AC-P-07: Artifact TTL maintenance

## 功能描述

在 ingest / AI process 启动时清理一批到期 inline artifact，形成 TTL 生命周期闭环。
权威契约：[plan/10 §3](../../plan/10-replay-and-backfill.md#3-raw_artifacts-留档策略)。

## 验收标准

- 单次调用最多 500 条，最旧优先，不循环排空；关闭新增留档不取消既有 expires_at。
- 永久保留、未来期限、file-backed 行不删除；SQLite 同秒精度差异不导致提前删除。
- 并发调用的实际删除计数各自有界；续期后保留，PG 跳过其他事务锁定的候选。
- PG 引用行被锁时，清理在 1 秒 lock timeout 后回滚；解除锁后的下一次调用可恢复。
- 外键置空，业务行与冻结报告保留；外键查找使用 0005 索引。
- 零删除静默；非零发出 artifacts_purged 事件；失败告警后继续运行。
- 旧版 0001–0004 数据库可升级到新增索引迁移，重复 apply 无副作用；undo 到 4 后原业务数据保留。
- 清理不会发生在 doctor / replay / rebuild-report / recent-entries / reindex dry-run 中。

## 测试覆盖

| 测试 | 文件 | 覆盖 |
|---|---|---|
| `ingest_start_purges_one_bounded_batch_even_when_retention_is_off` | `crates/runtime/tests/artifact_maintenance_tests.rs` | 启动 wiring / 500 上限 / 多次运行 |
| `ai_start_purges_artifacts_before_claim_and_records_actual_count` | 同上 | AI wiring / 零计数静默 / 事件 |
| `cleanup_failure_does_not_abort_ingest` | `crates/runtime/tests/artifact_maintenance_tests.rs` | 清理失败后继续主流程 |
| `pg_purge_times_out_on_locked_reference_and_recovers_without_partial_deletion` | `crates/storage/tests/raw_artifact_tests.rs` | 引用行锁超时 / 回滚 / 恢复 |
| `pg_purge_skips_concurrent_renewal_without_waiting_for_its_lock` | `crates/storage/tests/raw_artifact_tests.rs` | PG 并发续期锁 |
| `sqlite_purge_expired_is_bounded_and_preserves_ineligible_rows` | `crates/storage/tests/raw_artifact_tests.rs` | 到期、续期、NULL、file、同秒边界 |
| `pg_purge_expired_is_bounded_and_preserves_ineligible_rows` | 同上 | PG 对等边界（Docker fixture） |
| `sqlite_purge_clears_only_artifact_references_and_uses_indexes` | 同上 | 业务数据保留 / 外键 / EXPLAIN |
| `sqlite_concurrent_purgers_delete_disjoint_bounded_batches` | 同上 | 并发删除上限 |
| `sqlite_v0_7_1_database_upgrades_and_index_migration_rolls_back_without_data_changes` | `crates/storage/tests/release_compatibility_tests.rs` | 旧 DB → 当前 → 回退 |
| `pg_v0_7_1_database_upgrades_and_index_migration_rolls_back_without_data_changes` | 同上 | PG 对等迁移（Docker fixture） |

## 当前状态

`partial`

本地实现与验收已通过，最终提交 CI / PG16 Docker 验收尚未运行。

PG Docker fixture 默认 ignore，CI storage `--include-ignored` 执行；本轮本地额外使用隔离的原生
PostgreSQL 17 执行了升级、并发、SKIP LOCKED、引用保留及回退测试。实际命令和范围见本轮 handoff。
