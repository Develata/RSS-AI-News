# AC-P-05: 多方言存储（SQLite + PostgreSQL）

## 功能描述

`StoragePool` enum 统一封装 SQLite 与 PostgreSQL 两个底层 sqlx pool；同一套 `Repository` trait
在两个方言下行为对齐：claim+lease 并发模型、partial unique 行为、active rule resolver、
reindex transaction、raw_artifacts upsert 等关键写路径在两方言均测试覆盖。

面向场景：本地开发与生产部署可在不改 SQL 调用层的情况下切换 driver（详见 [../../plan/12-deployment.md](../../plan/12-deployment.md)）。

## 验收标准

### 命中条件（success path）

- migration 文件对：`migrations/sqlite/NNNN-name.sql` ↔ `migrations/postgres/NNNN-name.sql` **编号 + basename 一一对应**
- PostgreSQL 全量 migration 在 PG16 上可重复 apply 且每条反向迁移可干净 undo
- 两方言对核心 repo（feed_source / feed_entry / article / publish_record / rule_version / raw_artifact / run_event / article_ai_result / reindex_job）的 happy path 行为对齐
- claim 并发 → 多 worker 拿到**互不相交**的行集合
- release 用错误 owner → 返回 `false`、不动行
- lease 过期 → reclaim 清 owner 并允许重新 claim
- partial unique index `UNIQUE (kind) WHERE status='active'` 在两方言均生效
- migration 0002 backfill：把存量多 active 行降为 superseded，仅保留 `MAX(id)`；retired 行无论 id 如何均 demote

### 失败条件（failure path）

- 编号或 basename 不一致 → `migrations_sqlite_and_postgres_have_matching_numbers_and_basenames` 失败
- `Postgres` 未启用 `sqlx-postgres` feature 时 `StoragePool::Postgres` 路径在编译期就缺
- ai_result `(article_id, prompt_version_id)` 重复 → repo 返回 `None`（不抛错，让上层判定）
- article `content_hash` 重复 → `insert_or_get_by_content_hash` 返回**已有**行（幂等）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `migrations_sqlite_and_postgres_have_matching_numbers_and_basenames` | `crates/storage/tests/migration_pair_parity_tests.rs` | 双方言迁移对齐 |
| `postgres_migrations_apply_idempotent_and_undo_clean_on_pg16` | `crates/storage/tests/migrations_postgres_apply_tests.rs` | PG 迁移可重复 |
| `sqlite_happy_feed_source_upsert` / `pg_happy_feed_source_upsert` | `crates/storage/tests/dual_backend_smoke_tests.rs` | feed_source 双方言 |
| `sqlite_happy_reindex_insert_pending` / `pg_happy_reindex_insert_pending` | 同上 | reindex_job 双方言 |
| `sqlite_happy_article_insert_then_find` / `pg_happy_article_insert_then_find` | 同上 | article 双方言 |
| `sqlite_happy_publish_record_create` / `pg_happy_publish_record_create` | 同上 | publish_record 双方言 |
| `sqlite_happy_rule_version_get_or_create` / `pg_happy_rule_version_get_or_create` | 同上 | rule_version 双方言 |
| `sqlite_happy_raw_artifact_upsert_inline` / `pg_happy_raw_artifact_upsert_inline` | 同上 | raw_artifact 双方言 |
| `sqlite_happy_run_event_insert` / `pg_happy_run_event_insert` | 同上 | run_event 双方言 |
| `sqlite_happy_feed_entry_insert_then_find` / `pg_happy_feed_entry_insert_then_find` | 同上 | feed_entry 双方言 |
| `sqlite_happy_article_ai_result_insert_pending` / `pg_happy_article_ai_result_insert_pending` | 同上 | ai_result 双方言 |
| `parallel_claim_returns_disjoint_rows` | `crates/storage/tests/concurrency_tests.rs` | 并发不相交 |
| `release_with_wrong_owner_returns_false` | 同上 | owner 校验 |
| `reclaim_expired_lease_clears_owner_and_allows_reclaim` | 同上 | lease 回收 |
| `backfill_keeps_single_kind_single_row_as_active` | `crates/storage/tests/migration_0002_backfill_tests.rs` | 0002 backfill |
| `backfill_demotes_extra_rows_to_superseded_keeping_max_id_active` | 同上 | 多 active 降级 |
| `backfill_demotes_retired_rows_regardless_of_max_id` | 同上 | retired 也降级 |
| `partial_unique_index_holds_after_backfill` | 同上 | partial unique |
| `pg_fixture_creates_isolated_schema_with_full_migrations` | `crates/storage/tests/pg_fixture_smoke_tests.rs` | PG fixture |
| `pg_upsert_with_lease_guard_applied_writes_row_in_same_tx` | `crates/storage/tests/feed_source_pg_tests.rs` | PG lease guard |
| `pg_insert_pending_and_advance_article_atomically` | `crates/storage/tests/article_ai_result_pg_tests.rs` | PG 任务原子推进 |
| `pg_freeze_snapshot_inserts_publish_items_in_tx` | `crates/storage/tests/publish_record_pg_tests.rs` | PG freeze tx |
| `pg_rule_version_concurrent_seed_no_partial_unique_failure` | `crates/storage/tests/small_repos_pg_tests.rs` | PG partial unique 并发 |

## 当前状态

`passing`

CI `migrate` job 用 PG service container 端到端跑 PG 迁移 + 部分集成测试。

## 相关文档

- 设计：[../../plan/05-storage.md](../../plan/05-storage.md)
- 部署切换：[../../plan/12-deployment.md](../../plan/12-deployment.md) §PostgreSQL 切换
- 决策：`../../adr/0002-stage-driven-lease-claim.md`、`../../adr/0004-active-rule-resolver-partial-unique.md`、`../../adr/0005-storage-pool-dual-dialect.md`、`../../adr/0006-postgres-go-real-no-shrink.md`
