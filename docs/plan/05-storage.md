# 05 — 存储

本章详解存储层：schema 设计 / repo trait / claim+lease 机制 / 多方言 / migration / reindex。

存储层是宪法 §3.4 单一真相源的物理体现。所有跨流程的状态、配置、产物都落在这里。

## 1. 边界

本章覆盖：
- 11 张核心表的关系（feed_sources / feed_entries / articles / article_ai_results /
  publish_records / publish_items / raw_artifacts / rule_versions / reindex_jobs / run_events）
- `StoragePool` enum 与双方言抽象
- Repository trait 与 claim+lease SQL 模式
- SQLite ↔ PostgreSQL 翻译规则
- migrate + reindex 子命令

**不覆盖**：
- 业务状态机 → [./08-state-machines.md](./08-state-machines.md)
- 状态转移内的具体 SQL → 各能力章
- Migration 的运维细节 → [../operations/postgres-deployment.md](../operations/postgres-deployment.md)

## 2. 表关系一览

```text
feed_sources    ──┬─→ feed_entries  ──→  articles  ──┬─→ article_ai_results
                  │                                   │
                  │                                   ↓
                  │                              publish_items ←── publish_records
                  │
                  └─→ raw_artifacts (kind='feed_payload')
                         (kind='html_payload', target=feed_entry)
                         (kind='ai_raw_response', target=article_ai_result)

rule_versions ←── reindex_jobs  (status='active'|'pending'|'superseded')

run_events  (stage-scoped, append-only, 跨表事件)
```

每张表的 schema 与字段语义来自旧 design（详见 [`docs-backup/design/storage-schema.md`](../../docs-backup/design/storage-schema.md)，
迁移后归并到本节子条目）。

## 3. StoragePool（双方言）

`StoragePool` enum 在 [`crates/storage/src/lib.rs`](../../crates/storage/src/lib.rs) 定义：

```rust
pub enum StoragePool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}
```

唯一构造入口：`StoragePool::build(database_url) -> Result<Self, StorageError>`。
按 URL scheme 路由：
- `sqlite://` / `sqlite:` / 裸文件路径 → SQLite
- `postgres://` / `postgresql://` → PostgreSQL

CLI `--db-path` 与 `app.toml` 的 `[database].url` 都经过 `cli::db_url::resolve_storage_url`
完成 driver / URL 一致性校验，不一致 → exit 78 (`ConfigError`)。

详见 [../adr/0005-storage-pool-dual-dialect.md](../adr/0005-storage-pool-dual-dialect.md)。

## 4. Repository Trait 模式

每个本体对象 1 个 trait，定义在 `crates/storage/src/repo/<object>.rs`。统一形态：

```rust
#[async_trait]
pub trait XxxRepository: Send + Sync {
    async fn insert(&self, ...) -> Result<Xxx, StorageError>;
    async fn get(&self, id: &str) -> Result<Option<Xxx>, StorageError>;
    async fn list(&self, filter: ...) -> Result<Vec<Xxx>, StorageError>;
    async fn update_state(&self, id, expected, target, ...) -> Result<u64, StorageError>;
    // claim / lease（如适用）
    async fn claim_pending(&self, batch_size, lease_secs) -> Result<Vec<Xxx>, StorageError>;
    async fn reclaim_expired(&self) -> Result<u64, StorageError>;
}
```

每个 trait 由单个 `XxxRepo` struct 实现，内部持有 `StoragePool`；每个方法按
`match &self.pool` 派发到 `sqlite_*` / `pg_*` 私有 free fn（见 §4.1）。

### 4.1 文件组织约定（契约 / SQL / 实装 三件套）

较大的 repo 对象按**三件套**拆到同名前缀的 3 个文件，保持单文件 ≤800 行、契约与实装解耦：

| 文件 | 可见性 | 内容 |
|---|---|---|
| `<obj>.rs` | `pub mod` | 契约层：DTO struct/enum + `trait <Obj>Repository` + `<Obj>Repo` struct（`pub(super) pool: StoragePool`）+ 构造器 `new` / `new_with_storage` |
| `<obj>_sql.rs` | `mod`（私有） | SQL 层：`pub(super) const ... SQL`，方言等价的字符串集中于此 |
| `<obj>_impl.rs` | `mod`（私有） | 实装层：`#[async_trait] impl <Obj>Repository`（每方法 `match &self.pool` 派发）+ `sqlite_*` / `pg_*` 私有 free fn + row 解码 helper |

`repo/mod.rs` 对每个三件套对象声明 `pub mod <obj>; mod <obj>_impl; mod <obj>_sql;`，
并保持原 `pub use <obj>::{...}` 逐字不变——对外 API 与是否拆分无关。

**何时分裂方言 SQL**：绝大多数 const 跨方言逐字等价，单条 `pub(super) const` 共享即可。
仅当某条 SQL 在 PG 上需要 `FOR UPDATE SKIP LOCKED`（claim 子查询，§6.4 并发契约）而 SQLite
不支持时，才分裂为 `<NAME>_SQLITE_SQL` / `<NAME>_PG_SQL` 两条 const，由对应 `sqlite_*` /
`pg_*` helper 各自引用。

模板见 `publish_record{,_sql,_impl}.rs`；已套用：`reindex_job` / `article_ai_result` /
`feed_entry` / `feed_source`。小对象（`article` / `publish_item` / `raw_artifact` /
`rule_version` / `run_event`）仍单文件，无需强行三件套。

## 5. claim + lease 模式

所有需要并发 worker 的状态机统一用 claim+lease：

```sql
-- SQLite 版（BEGIN IMMEDIATE 串行化）
UPDATE feed_entries
SET state = 'fetching',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1
WHERE id IN (
  SELECT id FROM feed_entries
  WHERE state = 'pending_fetch'
    AND (lease_expires_at IS NULL OR lease_expires_at < CURRENT_TIMESTAMP)
  ORDER BY priority DESC, id
  LIMIT $3
)
RETURNING *;

-- PG 版（FOR UPDATE SKIP LOCKED）
UPDATE feed_entries
SET state = 'fetching', ...
WHERE id IN (
  SELECT id FROM feed_entries
  WHERE state = 'pending_fetch' AND (...)
  ORDER BY priority DESC, id
  LIMIT $3
  FOR UPDATE SKIP LOCKED
)
RETURNING *;
```

reclaim：

```sql
UPDATE feed_entries
SET state = 'pending_fetch',  -- 回滚到前置 pending
    lease_owner = NULL,
    lease_expires_at = NULL
WHERE state IN ('fetching', 'extracting')
  AND lease_expires_at < CURRENT_TIMESTAMP;
```

**`attempt_count` 不重置**，作为重试预算的硬约束。

涉及 lease 的表：`feed_entries` / `article_ai_results` / `publish_records` / `reindex_jobs`。

## 6. 多方言翻译规则

由 [`docs-backup/design/storage-multi-dialect.md`](../../docs-backup/design/storage-multi-dialect.md) §5
定义。关键差异：

| 维度 | SQLite | PostgreSQL |
|---|---|---|
| 占位符 | `$N` 或 `?`（统一用 `$N`） | `$N` |
| EXISTS 子查询 | `SELECT EXISTS(...)` returns 0/1 | 同左，用 `CASE WHEN ... THEN 1 ELSE 0 END` 包装解码 i32 |
| 时间函数 | 应用层 bind `OffsetDateTime::now_utc()` | 同左 |
| 自增主键 | `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGINT GENERATED BY DEFAULT AS IDENTITY` |
| Timestamp | TEXT (ISO 8601) | TIMESTAMPTZ |
| Binary | BLOB | BYTEA |
| Partial unique index | `CREATE UNIQUE INDEX ... WHERE ...` | 同 |
| Concurrency | `FOR UPDATE SKIP LOCKED` 不支持，用 `BEGIN IMMEDIATE` 串行 | `FOR UPDATE SKIP LOCKED` |

详见 [../adr/0006-postgres-go-real-no-shrink.md](../adr/0006-postgres-go-real-no-shrink.md)。

## 7. Migration

`crates/storage/src/lib.rs::migrate` 是入口：

```rust
pub async fn run(pool: &StoragePool) -> Result<MigrationReport, StorageError>;
pub async fn check(pool: &StoragePool) -> Result<MigrationStatus, StorageError>;
```

按 `StoragePool` 派发到 `migrations/sqlite/` 或 `migrations/postgres/`。

### 7.1 当前 migration 文件

| Version | 内容 | SQLite 文件 | PG 文件 |
|---|---|---|---|
| 0001 | 全部 11 张表 + 索引 + 约束 | `migrations/sqlite/0001_init.{up,down}.sql` | `migrations/postgres/0001_init.{up,down}.sql` |
| 0002 | reindex_jobs + rule_versions.status | `migrations/sqlite/0002_reindex_jobs_and_rule_status.{up,down}.sql` | `migrations/postgres/0002_reindex_jobs_and_rule_status.{up,down}.sql` |

### 7.2 migrate 子命令

- `migrate run` — 执行 pending migration（必须由 driver=URL 与配置 driver 一致）
- `migrate check` — 检查版本状态，不执行

`migrate` **不**经过 RunContext / Flow 编排，直接调 storage 层。`validate-config` 同样。

## 8. reindex

版本化规则升级（prompt / link_hash 算法 / categories）通过 reindex 流程：

```text
1. INSERT rule_versions (status='pending')
2. INSERT reindex_jobs (state='pending')
3. claim batch → state='running'，更新数据行 *_rule_version_id 指向 pending
4. checkpoint 每批 commit + last_processed_id
5. 全部完成 → 事务内：rule_versions pending → active，旧 active → superseded
6. reindex_jobs → completed
```

三类 target：
- `link_hash` — 重算所有 feed_entries.link_hash（升级 normalizer 算法时）
- `content_hash` — 重算所有 articles.content_hash（升级正文规范化算法时）
- `categories` — 重算 article→category 关联（升级分类规则时）

active rule resolver 保证 reindex 中途 `active_rule(kind)` 仍返回旧 active 行，不污染读路径。
详见 [../adr/0004-active-rule-resolver-partial-unique.md](../adr/0004-active-rule-resolver-partial-unique.md)。

### 8.1 并发与失败恢复

- partial unique index `UNIQUE (target) WHERE state IN ('pending', 'running')` 保证同 target
  只能有一个 active job
- lease 过期 reclaim → 保留 last_processed_id checkpoint，下轮从 checkpoint resume
- crash-after-batch：已 commit 批次保留，未 commit 丢失，从 checkpoint 重做

测试覆盖：[`crates/runtime/tests/`](../../crates/runtime/tests/) 中的 reindex_* 测试。

## 9. raw_artifacts

留档原始外部输入，支持回放。三种 kind：
- `feed_payload` — feed XML/JSON 原文
- `html_payload` — 详情页 HTML 原文
- `ai_raw_response` — LLM 原始响应

写入由 `runtime/src/artifact.rs::ArtifactWriter` 完成。retention policy 控制写入时机与生命周期，详见 [./10-replay-and-backfill.md](./10-replay-and-backfill.md)。

## 10. run_events

跨表 append-only 事件流。stage / target 维度聚合关键事件：

```sql
CREATE TABLE run_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    stage TEXT NOT NULL,           -- 'ingest' / 'extract' / 'ai_run' / 'publish' / 'reindex'
    kind TEXT NOT NULL,            -- 'source_fetch_succeeded' / 'entry_dedup_skipped' / ...
    target_kind TEXT,              -- 'feed_source' / 'feed_entry' / 'article' / ...
    target_id TEXT,
    context_json TEXT,             -- 自由结构
    created_at TEXT NOT NULL
);
```

写入由 `runtime::events::RunEventEmitter` 完成。详见 [./07-observability.md](./07-observability.md)。

## 11. 当前实现入口

| 内容 | 路径 |
|---|---|
| StoragePool | [`crates/storage/src/lib.rs`](../../crates/storage/src/lib.rs) |
| Repository（trait + SQL + 实装，较大对象拆三件套见 §4.1） | [`crates/storage/src/repo/`](../../crates/storage/src/repo/) |
| SQLite migrations | [`migrations/sqlite/`](../../migrations/sqlite/) |
| PostgreSQL migrations | [`migrations/postgres/`](../../migrations/postgres/) |
| StorageError | [`crates/storage/src/error.rs`](../../crates/storage/src/error.rs) |
| migrate CLI | [`crates/cli/src/commands/migrate.rs`](../../crates/cli/src/commands/migrate.rs) |
| reindex Flow | [`crates/runtime/src/flows/reindex.rs`](../../crates/runtime/src/flows/reindex.rs) |
| db url resolver | [`crates/cli/src/db_url.rs`](../../crates/cli/src/db_url.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。
