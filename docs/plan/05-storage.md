# 05 — 存储

本章详解存储层：schema 设计 / repo trait / claim+lease 机制 / 多方言 / migration / reindex。

存储层是宪法 §3.4 单一真相源的物理体现。所有跨流程的状态、配置、产物都落在这里。

## 1. 边界

本章覆盖：
- 12 张用户表的关系（feed_sources / feed_entries / articles / article_ai_results /
  publish_records / publish_items / raw_artifacts / rule_versions / reindex_jobs / run_events /
  decision_policies / decision_results），另有 sqlx 自管的 `_sqlx_migrations`
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

decision_policies ←── decision_results ──→ articles   (typed-decision shadow，ADR 0011)
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

### 4.2 feed entry 原子 link dedup

`FeedEntryRepository::insert_deduplicated` 以单条 `INSERT .. ON CONFLICT DO NOTHING` 同时处理
`(source_id, feed_entry_uid)` 与 canonical `link_hash` uniqueness，并返回
`Inserted / UidDuplicate / LinkDuplicate` typed outcome。migration 0004 新增
`link_dedup_shadow`：每组历史重复 `link_hash` 按最小 id 保留一条 canonical，其余行保留原数据并
标 shadow；partial unique index 仅约束 canonical rows。由此跨源并发写入由数据库唯一约束裁决，
不再存在 SELECT→INSERT TOCTOU 窗口。

**何时分裂方言 SQL**：绝大多数 const 跨方言逐字等价，单条 `pub(super) const` 共享即可。
仅当某条 SQL 在 PG 上需要 `FOR UPDATE SKIP LOCKED`（claim 子查询，§6.4 并发契约）而 SQLite
不支持时，才分裂为 `<NAME>_SQLITE_SQL` / `<NAME>_PG_SQL` 两条 const，由对应 `sqlite_*` /
`pg_*` helper 各自引用。

**实装层再分方言（仅当 `<obj>_impl.rs` 超 800 行）**：默认 `sqlite_*` 与 `pg_*` free fn
同住 `<obj>_impl.rs`（与 `match &self.pool` 派发同文件，便于对照）。仅当实装层自身超
800 行软上限时，才把它再拆为 `<obj>_impl.rs`（仅留 trait 派发）+ `<obj>_impl_sqlite.rs`
+ `<obj>_impl_pg.rs`（各持 `pub(super)` 方言 free fn，由派发层显式 `use` 调用）。命名用
`_impl_sqlite` / `_impl_pg` 以与 `_sql.rs`（SQL 常量）区分。当前仅 `reindex_job` 适用。

模板见 `publish_record{,_sql,_impl}.rs`；已套用：`reindex_job` / `article_ai_result` /
`feed_entry` / `feed_source`。小对象（`article` / `publish_item` / `raw_artifact` /
`rule_version` / `run_event`）仍单文件，无需强行三件套。

## 5. claim + lease 模式

所有需要并发 worker 的状态机统一用 claim+lease：

```sql
-- SQLite 版（单条 UPDATE … WHERE id IN (SELECT …) 语句，依赖 SQLite 单写者 + busy_timeout）
UPDATE feed_entries
SET state = 'fetching',
    lease_owner = $1,
    lease_expires_at = $2,
    attempt_count = attempt_count + 1
WHERE id IN (
  SELECT id FROM feed_entries
  WHERE state = 'pending_fetch'
    AND (lease_expires_at IS NULL OR julianday(lease_expires_at) < julianday($now_ms_floor))
    AND attempt_count < $max_attempts
    AND (next_attempt_at IS NULL OR julianday(next_attempt_at) <= julianday($now_ms_floor))
    AND id NOT IN (SELECT value FROM json_each($exclude_ids))
  ORDER BY discovered_at ASC
  LIMIT $batch_size
)
RETURNING *;

-- PG 版（FOR UPDATE SKIP LOCKED）
UPDATE feed_entries
SET state = 'fetching', ...
WHERE id IN (
  SELECT id FROM feed_entries
  WHERE state = 'pending_fetch' AND (...)          -- 同上；冷却直接比较 TIMESTAMPTZ，
    AND id <> ALL($exclude_ids::BIGINT[])          -- 排除列表为数组
  ORDER BY discovered_at ASC
  LIMIT $batch_size
  FOR UPDATE SKIP LOCKED
)
RETURNING *;
```

候选子查询 JOIN 了其它表时（如 AI claim 连接 `articles` / `feed_entries` / `feed_sources` 以按分类过滤），
PG 必须写 `FOR UPDATE OF <被领取表别名> SKIP LOCKED`（AI：`OF aar`）。不带 `OF` 会锁住并跳过所有
JOIN 表的行，另一事务锁住某个 source / article 行时，本可领取的任务会被当作"已锁"跳过，worker 把空批次
当成队列耗尽提前结束（测试 `pg_claim_is_not_hidden_by_locked_source_or_article_rows`）。

reclaim：

```sql
UPDATE feed_entries
SET state = 'pending_fetch',  -- 回滚到前置 pending
    lease_owner = NULL,
    lease_expires_at = NULL
WHERE state IN ('fetching', 'extracting')
  AND lease_expires_at < $now;        -- PG；SQLite 见下段（julianday + 文本回退）
```

不要在 SQLite 上用 `CURRENT_TIMESTAMP` 比较这些列：它是 `YYYY-MM-DD HH:MM:SS`（空格分隔），
与 sqlx 写入的 RFC3339（`T` 分隔）按文本比较时同一天的行会全部判错。

SQLite 以小数位数可变的 RFC3339 TEXT 存时间，同一秒内字典序 ≠ 时刻序（`…00.12Z` <
`…00.1Z`），故 `feed_entries` 与 `publish_records`（claim / 按 id claim / reclaim / `terminalize_exhausted`）在 SQLite 上用
`julianday()` 比较 lease：比较侧 now 转 UTC 并向下取整到毫秒，claim 写入的
`lease_expires_at` 转 UTC 并向上取整到毫秒；严格 `<` 加上 `julianday()` 的单调舍入，保证
lease 绝不提前判过期（最多晚约 2 ms）。`julianday()` 解析不了的文本（例如 `+15:00` 偏移）
回退到原先的文本比较，避免行被永久跳过。PG 直接比较 `TIMESTAMPTZ`。
`article_ai_results`（claim / reclaim / `terminalize_exhausted`）与 `decision_results`（claim /
reclaim / `terminalize_exhausted` / 原始响应 TTL 清理的 `raw_response_expires_at`）2026-11-21 起
采用同一规则（`repo::sqlite_stored_deadline` 写入、`repo::sqlite_instant_cutoff` 比较，
测试 `ai_decision_lease_instant_tests.rs`）。其余 lease 表（`reindex_jobs`）与 doctor 的同类谓词仍是
字典序比较，待单独修复。

**`attempt_count` 不重置**，作为重试预算的硬约束。publish_records 例外：预算读 `stage_attempt_count`（阶段推进清零），`attempt_count` 只累计审计（[11 §4.1](./11-error-and-recovery.md)）。

涉及 lease 的表：`feed_entries` / `article_ai_results` / `publish_records` / `reindex_jobs` / `decision_results`（0006，见 ADR 0011）。

### 5.1 重试预算耗尽闭环（W15）

retryable release 在 SQL 内按预算折叠，杜绝"耗尽后回到可领取态但永远不被 claim"的卡死：

```sql
UPDATE <表>
SET state = CASE WHEN attempt_count >= $max THEN '<终态>' ELSE '<可领取态>' END,
    lease_owner = NULL, lease_expires_at = NULL, last_error = $..., ...
WHERE id = $... AND lease_owner = $...
RETURNING state;  -- 调用方据此判别 ReleaseFailureOutcome { released, exhausted }
```

辅以 `terminalize_exhausted` 兜底 sweep（可领取态 + `attempt_count >= max` + lease
空/过期 → 终态；`COALESCE` 保留已有 `last_error*`，缺失时填 `retry budget exhausted`），
覆盖"崩溃发生在最后一次尝试"的路径。`feed_entries` 的 retryable release 还在同一条
UPDATE 内写 `next_attempt_at`，`claim_pending_fetch` 跳过截止点晚于 cutoff 的行（0007，
[./02-extract.md](./02-extract.md) §5.3）。reindex 无预算语义（claim 不过滤
attempt_count，失败直转终态），只接 reclaim。接线时机、事件与恢复路径见
[./15-retry-exhaustion-and-reclaim.md](./15-retry-exhaustion-and-reclaim.md) §5–§7。

## 6. 多方言翻译规则

由 [`docs-backup/design/storage-multi-dialect.md`](../../docs-backup/design/storage-multi-dialect.md) §5
定义。关键差异：

| 维度 | SQLite | PostgreSQL |
|---|---|---|
| 占位符 | `$N` 或 `?`（统一用 `$N`） | `$N` |
| EXISTS 子查询 | `SELECT EXISTS(...)` returns 0/1 | 同左，用 `CASE WHEN ... THEN 1 ELSE 0 END` 包装解码 i32 |
| 时间函数 | 应用层 bind `OffsetDateTime::now_utc()` | 同左 |
| 自增主键 | `INTEGER PRIMARY KEY`（rowid 别名，不用 `AUTOINCREMENT`） | `BIGINT GENERATED BY DEFAULT AS IDENTITY` |
| Timestamp | TEXT (ISO 8601) | TIMESTAMPTZ |
| Binary | BLOB | BYTEA |
| Partial unique index | `CREATE UNIQUE INDEX ... WHERE ...` | 同 |
| Concurrency | 不支持 `FOR UPDATE SKIP LOCKED`；claim 为单条条件 `UPDATE … RETURNING`（批量领取通常为 `WHERE id IN (SELECT … LIMIT n)`，reindex 等用标量子查询或按 id 直接更新），写入由 SQLite 单写者锁串行，等待上限为连接的 `busy_timeout`（WAL 模式，见 `pool.rs`）；仓库中无 `BEGIN IMMEDIATE` | `FOR UPDATE SKIP LOCKED` |

详见 [../adr/0006-postgres-go-real-no-shrink.md](../adr/0006-postgres-go-real-no-shrink.md)。

## 7. Migration

入口在 [`crates/storage/src/migrate.rs`](../../crates/storage/src/migrate.rs)（经 `lib.rs` 重导出）：

```rust
pub async fn run_migrations(pool: &StoragePool) -> Result<(), StorageError>;
pub fn embedded_migration_versions(pool: &StoragePool) -> Vec<i64>;
pub async fn applied_migration_versions(pool: &StoragePool) -> Result<Vec<i64>, StorageError>;
pub fn pending_migration_versions(pool: &StoragePool, applied: &[i64]) -> Vec<i64>;
pub async fn ensure_migration_state_exact(pool: &StoragePool) -> Result<(), StorageError>;
```

- `run_migrations` 按 `StoragePool` 派发到 `sqlx::migrate!("../../migrations/sqlite")` 或
  `("../../migrations/postgres")`，迁移集在编译期内嵌。
- `embedded_migration_versions` / `applied_migration_versions` / `pending_migration_versions`：
  内嵌版本、`_sqlx_migrations` 中 `success` 的版本（表不存在按 0 applied）、二者之差。
- `ensure_migration_state_exact`：数据库迁移历史必须与内嵌的 up 迁移在长度、版本、checksum、
  success 上逐行一致，否则 fail closed。

已提交的迁移文件字节不可变（sqlx checksum；pin 测试见 `crates/storage/tests/migration_immutability_tests.rs`），
schema 变更只能新增迁移，且 SQLite / PostgreSQL 两个方言同时提供。

### 7.1 当前 migration 文件

两方言文件名一致：`migrations/sqlite/<name>.{up,down}.sql` 与 `migrations/postgres/<name>.{up,down}.sql`。

| Version | 文件名 | 内容 |
|---|---|---|
| 0001 | `0001_init` | 9 张表（rule_versions / raw_artifacts / feed_sources / articles / feed_entries / article_ai_results / publish_records / publish_items / run_events）+ 索引 + 约束 |
| 0002 | `0002_reindex_jobs_and_rule_status` | reindex_jobs 表 + `rule_versions.status` 与 active partial unique index |
| 0003 | `0003_ai_effective_model_id` | `article_ai_results.effective_model_id`（非键列，记实际生效模型） |
| 0004 | `0004_feed_entry_link_dedup_atomicity` | feed entry canonical / shadow 标记 + link_hash partial unique index（§4.2） |
| 0005 | `0005_artifact_reference_indexes` | raw artifact 引用列索引（artifact 过期时的反查） |
| 0006 | `0006_decision_results` | decision_policies / decision_results（typed-decision shadow，[ADR 0011](../adr/0011-decision-run-shadow-runner.md)） |
| 0007 | `0007_feed_entry_retry_cooldown` | `feed_entries.next_attempt_at`：extract retryable 失败的冷却截止点（[02-extract §5.3](./02-extract.md)） |
| 0008 | `0008_publish_stage_budget_and_item_article_index` | `publish_records.stage_attempt_count`（按阶段计的发布重试预算，存量行为 0）+ `idx_publish_items_article`（快照归属查询）+ `publish_records.next_attempt_at`（限流后的最早重试时刻，claim 遵守，R1-P-S12）；[ADR 0012](../adr/0012-per-stage-publish-budget-and-snapshot-ownership.md) |

### 7.2 migrate 子命令

- `migrate run` — 先确认无 running / pending reindex job，再 `run_migrations`（driver=URL 必须与配置 driver 一致）
- `migrate check` — 不执行迁移：`applied_migration_versions` 对比内嵌版本，存在 pending 即 `MigrateCheckPending`（exit 1）。
  它不是无副作用的：打开 SQLite 连接时仍会按 `pool.rs` 在路径不存在时创建数据库文件，并把 journal 设为 WAL（WAL 设置持久）

`migrate` **不**经过 Flow 编排，直接调 storage 层。`validate-config` 同样。

## 8. reindex

reindex作用于全局规则与数据，CLI拒绝 `--category`（exit 2）；特别是categories归档判断必须使用完整配置。

版本化规则升级（link_hash / content_hash 算法 / categories）通过 reindex 流程。reindex 不处理
prompt：`prompt_version` 语义见 [./03-ai.md](./03-ai.md) §3.2，历史文章用新 prompt 重跑走
`backfill --target ai`（[./10-replay-and-backfill.md](./10-replay-and-backfill.md) §5.2）。

```text
1. INSERT rule_versions (status='pending')
2. INSERT reindex_jobs (state='pending')
3. claim batch → state='running'，更新数据行 *_rule_version_id 指向 pending
4. checkpoint 每批 commit + last_processed_id
5. 全部完成 → 事务内：rule_versions pending → active，旧 active → superseded
6. reindex_jobs → completed
```

三类 target：
- `link_hash` — 重算所有 feed_entries.link_hash（升级 normalizer 算法时）；每次 move 在事务内
  串行化写路径，对 old/new groups 按最小 id 原子重选 canonical；candidate 非最小 id 时计入
  `conflict_skipped`，collision chain 不留下 shadow-only group
- `content_hash` — 重算所有 articles.content_hash（升级正文规范化算法时）
- `categories` — 重算 article→category 关联（升级分类规则时）

active rule resolver 保证 reindex 中途 `active_rule(kind)` 仍返回旧 active 行，不污染读路径。
详见 [../adr/0004-active-rule-resolver-partial-unique.md](../adr/0004-active-rule-resolver-partial-unique.md)。

### 8.1 并发与失败恢复

- partial unique index `UNIQUE (target) WHERE state IN ('pending', 'running')` 保证同 target
  只能有一个 active job
- lease 过期 reclaim → 保留 last_processed_id checkpoint，下轮从 checkpoint resume
- 续跑入口（2026-11-21）：每次 `reindex` 先在单事务内查该 target 的活动 job（`pending`，或 `running`
  但 lease 已过期——reclaim 是 best-effort），规则 `kind` + `payload_sha256` 与本次请求一致即由本 worker
  领取并从 `last_processed_id` 之后继续（`reindex_resumed` 事件）；没有活动 job 才新建
  （`start_reindex_tx`）。`version_tag` 只是 CLI 每次自动生成的标签（`reindex-<target>-<unix-ts>`），
  不参与匹配，否则普通重跑永远无法续跑。规则不一致或另一 worker 持有存活 lease → 显式
  `StorageError::Conflict { table: "reindex_jobs" }`，信息含 `reindex --abort <id>`，不改任何行。
- 每批一个事务：先执行 owner-guarded 的 checkpoint UPDATE（`last_processed_id` + 续租
  `lease_expires_at`，同时取得 SQLite 写锁 / PG job 行锁，使 abort / reclaim 与本批串行化），再改写本批
  业务行；owner 校验失败整批回滚，已失去 lease 的 worker 不会改动任何业务行。PG 的单行冲突用事务内预检
  （不靠捕获失败语句，失败语句会中止整个 PG 事务）。
- crash-after-batch：已 commit 批次保留，未 commit 丢失，从 checkpoint 重做

测试覆盖：[`crates/runtime/tests/`](../../crates/runtime/tests/) 中的 reindex_* 测试。

### 8.2 `kind='config'` 行的生命周期（W16）

config 行**不走** reindex 两阶段激活，由 CLI 启动期 seed 直接轮换
（`RuleVersionRepo::rotate_active_config`）：active 行 `payload_sha256` 与当前
`config_sha256` 一致 → 零写入；漂移 → 单事务 demote 旧 active → 按 payload
复用既有行（pending / superseded 复活，清 `retired_at`）或插入新行（tag =
sha 前 12 位）。仅该 kind 开放 superseded → active 复活（回滚 A→B→A）。
完整设计与决策见 [./16-config-versioning.md](./16-config-versioning.md)。

## 9. raw_artifacts

留档原始外部输入，支持回放。三种 kind：
- `feed_payload` — feed XML/JSON 原文
- `html_payload` — 详情页 HTML 原文
- `ai_raw_response` — LLM 原始响应

写入由 `runtime/src/artifact.rs::ArtifactWriter` 完成。retention policy 控制写入时机与生命周期，详见 [./10-replay-and-backfill.md](./10-replay-and-backfill.md)。

## 10. run_events

跨表 append-only 事件流。stage / target 维度聚合关键事件：

```sql
CREATE TABLE run_events (           -- SQLite 方言，0001_init
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL,
    trace_id TEXT NULL,
    stage TEXT NOT NULL,
    severity TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    target_kind TEXT NULL,
    target_id INTEGER NULL,
    message TEXT NOT NULL,
    context_json TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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
| reindex Flow | [`crates/runtime/src/flows/reindex/`](../../crates/runtime/src/flows/reindex/) |
| db url resolver | [`crates/cli/src/db_url.rs`](../../crates/cli/src/db_url.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。

## 边界加固（2026-09-10）

Domain 不依赖 SQLx：状态枚举与 Score0To100 只表达纯业务语义。
Storage 将原始整数显式转换为 Score0To100，坏数据拒绝并保留越界值，不作截断。
SQLite/PG 的 atomic claim、unique dedup、lease、retry 与事务实现保持双方言显式分支。

数据库 URL 的 `://` scheme 只允许 sqlite/postgres/postgresql；未知 scheme 明确拒绝，
不会当作本地文件创建。保留 sqlite:、sqlite://、裸路径，scheme 比较不区分大小写。
错误不回显可能包含密码的 URL。AI taskgen 扫描仅返回 article_id，不读取正文或标题。
本轮不新增 migration/index/batch insert；SQLite 查询计划与 perf 证据见本轮工程报告。
