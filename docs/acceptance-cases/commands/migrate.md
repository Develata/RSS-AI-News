# AC-C-06: migrate 子命令

## 功能描述

执行 schema 迁移。两个子动作：
- `migrate run`：应用 embedded migrations 到当前 DB（SQLite 或 PostgreSQL）
- `migrate check`：仅校验 embedded 与 DB 已 applied 的版本一致，不写入

migrate 是基础设施命令，**不**走完整 `validate-config`（不要求 OPENAI / RSSHUB 等业务 env）；
只跑结构性 schema 检查（`run_structural_checks`）。

面向场景：首次部署、版本升级前的 schema 一致性确认。

## 验收标准

### 命中条件（success path）

- `migrate run` 在空 DB 上执行 → 应用全部 embedded migrations
- 在已迁移 DB 上 `migrate run` → idempotent，无新动作
- `migrate check` 在版本一致时 → exit 0
- SQLite ↔ PostgreSQL 同编号 + basename 的 migration 文件一一对应
- PG 迁移全套在 PG16 上可重复 apply 且每条反向迁移可干净 undo
- migration 0002 backfill 把存量多 active 行降为 superseded
- 即使缺 `OPENAI_API_KEY` 等业务 env，migrate 仍可运行
- summary 输出 current_version

### 失败条件（failure path）

- `migrate check` 版本不一致 → exit 1 + diff 报告
- DB 不可达 / 权限不足 → `StorageError`，exit 1
- migration 文件本身 SQL 错 → 失败回滚
- partial unique index 在 0002 之后必须仍然成立（任何重复 active 必被 0002 修复或失败）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `args_parsing_parses_migrate_run_subcommand` | `crates/cli/tests/args_parsing_tests.rs` | `run` 解析 |
| `args_parsing_parses_migrate_check_subcommand` | 同上 | `check` 解析 |
| `migrate_summary_pretty_renders` | `crates/cli/tests/w9c_cli_tests.rs` | summary pretty |
| `migrate_summary_serializes_current_version` | 同上 | summary JSON |
| `migrations_sqlite_and_postgres_have_matching_numbers_and_basenames` | `crates/storage/tests/migration_pair_parity_tests.rs` | 双方言对齐 |
| `postgres_migrations_apply_idempotent_and_undo_clean_on_pg16` | `crates/storage/tests/migrations_postgres_apply_tests.rs` | PG 迁移 |
| `backfill_keeps_single_kind_single_row_as_active` | `crates/storage/tests/migration_0002_backfill_tests.rs` | 0002 backfill |
| `backfill_demotes_extra_rows_to_superseded_keeping_max_id_active` | 同上 | 0002 多 active 降级 |
| `backfill_demotes_retired_rows_regardless_of_max_id` | 同上 | retired 降级 |
| `backfill_independently_handles_multiple_kinds` | 同上 | 多 kind 独立 |
| `partial_unique_index_holds_after_backfill` | 同上 | partial unique |
| `fresh_db_with_no_existing_rows_applies_0002_cleanly` | 同上 | 空 DB 应用 |
| `pg_fixture_creates_isolated_schema_with_full_migrations` | `crates/storage/tests/pg_fixture_smoke_tests.rs` | PG 全量 fixture |

## 当前状态

`passing`

CI `migrate` job 用 PG service container 端到端跑 migrate run。

## 相关文档

- 设计：[../../plan/05-storage.md](../../plan/05-storage.md) §7 migration
- 多方言：[../pipelines/05-multi-dialect-storage.md](../pipelines/05-multi-dialect-storage.md)
- 部署：[../../plan/12-deployment.md](../../plan/12-deployment.md) §首次 migrate
- 决策：`../../adr/0005-storage-pool-dual-dialect.md`
