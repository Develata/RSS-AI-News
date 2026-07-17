# AC-C-09: recent-entries 子命令

## 功能描述

`recent-entries` 从已存在的 RSS-AI-News 数据库导出一个 category 在指定发现时间之后的最近 feed entries，并附带 active source 的安全健康摘要。它面向下游 discovery consumer；不抓网络、不推进状态、不运行 AI、不发布，也不把 feed 内容当作原文证据。

```bash
rss-ai-news \
  --config-dir configs \
  --category daily-math \
  --output-format json \
  recent-entries \
  --discovered-after 2026-07-14T23:30:00Z \
  --limit 50
```

该命令是固定领域 projection，不是 arbitrary SQL 接口。它不复用会自动 migration/config rotation 的完整 `RunContext`，而使用轻量 read-only dependencies。

## Public contract

### 参数

- 全局 `--category/-C`：本命令必填；缺失为 user error。
- `--discovered-after <RFC3339>`：必填，按 `discovered_at >= value` inclusive 过滤。
- `--limit <1..=200>`：默认 50；内部读取 `limit + 1` 判断 `truncated`。
- 全局 `--dry-run`：合法 no-op；本命令本身已经严格只读。
- v1 不提供 OFFSET/cursor、state filter、source filter 或 `published_at` filter。

### 数据选择

```sql
FROM feed_entries fe
JOIN feed_sources fs ON fs.id = fe.source_id
WHERE fs.category_key = :category
  AND fs.status = 'active'
  AND fe.discovered_at >= :discovered_after
  AND fe.state <> 'dedup_skipped'
ORDER BY fe.discovered_at DESC, fs.priority ASC, fe.id DESC
LIMIT :limit_plus_one
```

`published_at` 可空且来源时钟不可信，只作为输出字段，不作为默认时间窗口或排序键。SQLite 中 sqlx 把时间编码为变长 RFC3339 TEXT；实现必须以 instant 语义精确比较/排序，不能直接依赖 TEXT lexical order，并以一日 coarse lower bound 继续利用现有 `(source_id, discovered_at)` index。

active source health 使用独立固定 projection，最多返回 500 行；内部读取 501 行判断 `source_health_truncated`。`source_key` 与 `last_error_kind` 分别最多输出 256/128 个数据库字符。

### JSON summary schema v1

现有 `OutputWriter` envelope 保持不变：

```json
{
  "command": "recent-entries",
  "status": "success",
  "summary": {
    "schema_version": 1,
    "generated_at": "2026-07-17T23:30:00Z",
    "category": "daily-math",
    "discovered_after": "2026-07-14T23:30:00Z",
    "limit": 50,
    "truncated": false,
    "source_health_truncated": false,
    "source_health": [
      {
        "source_key": "person.terence-tao.whats-new",
        "priority": 10,
        "last_fetched_at": null,
        "last_success_at": null,
        "consecutive_failures": 0,
        "last_error_kind": null
      }
    ],
    "entries": [
      {
        "id": 1,
        "source_key": "person.terence-tao.whats-new",
        "source_priority": 10,
        "title": "Example",
        "url": "https://example.com/post",
        "published_at": null,
        "discovered_at": "2026-07-17T22:00:00Z",
        "state": "pending_fetch"
      }
    ]
  },
  "errors": []
}
```

v1 不输出 `summary_raw`、完整 `last_error`、feed secret、AI result、score 或 publish state。

## 严格 read-only 不变量

1. SQLite DB 不存在时不得创建文件。
2. 不自动运行 migration，不新增/更新 `rule_versions`，不写 run events。
3. 不更新 feed entry/source state、lease、attempt、timestamp 或 error fields。
4. 查询前 migration state 必须 exact fail closed：拒绝 pending、failed、unknown extra version 与 checksum drift，并提示先显式运行 `migrate run`。
5. SQLite 使用 open-existing read-only connection；WAL 模式允许 SQLite 创建/更新 `-wal/-shm` coordination sidecars，但 DB main file、config 与逻辑 rows 必须不变，且任何 SQL write 必须返回 readonly error。PG 每个 command connection 设置 `default_transaction_read_only = on`，且 command path 只执行 SELECT。
6. entry 输出内存受 `limit <= 200` 约束，entry query 至多保留 201 行；source health 独立受 500+1 行和字段长度上限约束。

## 验收矩阵

| ID | 层 | 场景 | 预期 | Rust evidence |
|---|---|---|---|---|
| RE-CLI-001 | args | 合法 RFC3339 + 默认 limit | 解析为 `RecentEntriesArgs` | `args_parsing_parses_recent_entries_defaults` |
| RE-CLI-002 | args | 非法 timestamp | clap exit 2 | `args_parsing_rejects_recent_entries_invalid_timestamp` |
| RE-CLI-003 | args | limit=0 / 201 | clap exit 2 | `args_parsing_rejects_recent_entries_limit_out_of_range` |
| RE-CLI-004 | command | 缺 `--category` | user error，command=`recent-entries` | `recent_entries_requires_category` |
| RE-CLI-005 | command | `--dry-run` | 与普通查询输出等价，不写库 | `recent_entries_dry_run_is_read_only_noop` |
| RE-DB-001 | storage | category A/B 混合 | 只返回指定 category + active source | `recent_entries_filters_category_and_active_sources` |
| RE-DB-002 | storage | 边界时间 | `>= discovered_after`，更早排除 | `recent_entries_uses_inclusive_discovered_after` |
| RE-DB-003 | storage | 多 source 同时刻 | `discovered_at DESC, priority ASC, id DESC` | `recent_entries_order_is_deterministic` |
| RE-DB-004 | storage | `dedup_skipped` 与其它 state | 仅排除 `dedup_skipped` | `recent_entries_excludes_dedup_skipped` |
| RE-DB-005 | storage | limit N 且 N+1 rows | 返回 N，`truncated=true` | `recent_entries_limit_plus_one_sets_truncated` |
| RE-DB-006 | storage | `Z`/offset/变长 fraction | 按 instant 精确过滤与排序 | `recent_entries_sqlite_orders_fractional_and_offset_timestamps_by_instant` |
| RE-SRC-001 | runtime | 501 active sources + oversized error kind | 返回 500，health truncated，字段有界 | `recent_entries_source_health_is_bounded_and_truncated` |
| RE-RO-001 | pool | SQLite path 不存在 | 失败且文件仍不存在 | `read_only_sqlite_pool_does_not_create_missing_db` |
| RE-RO-002 | pool | read-only SQLite 执行 UPDATE | 返回 readonly error | `read_only_sqlite_pool_rejects_writes` |
| RE-RO-003 | command | migration pending | exit 1，不 apply | `recent_entries_fails_when_migration_pending` |
| RE-RO-004 | migration | failed row / extra version / checksum drift | exact fail closed | `exact_migration_state_rejects_failed_rows_extra_versions_and_checksum_drift` |
| RE-RO-005 | command | 查询前后 fixture | DB main/config 不变；仅允许 SQLite coordination sidecars | `recent_entries_read_path_does_not_mutate_database` |
| RE-RO-006 | command | unknown migration version | command/error kind 正确并提示 migrate | `recent_entries_fails_closed_on_unknown_migration_version` |
| RE-JSON-001 | output | success JSON | envelope/schema/字段稳定 | `recent_entries_json_envelope_matches_contract` |
| RE-JSON-002 | output | source 有完整 error/summary | 不输出 `last_error` / `summary_raw` | `recent_entries_output_redacts_large_or_sensitive_fields` |
| RE-PG-001 | storage | PG native `TIMESTAMPTZ` offset/fraction/boundary fixture | 行集合、instant 过滤与排序同 SQLite | `pg_recent_entries_matches_sqlite_contract` |
| RE-PG-002 | pool | `build_read_only` PG pool | `default_transaction_read_only=on`；`UPDATE` 返回 SQLSTATE `25006`；数据不变 | `pg_recent_entries_read_only_pool_enforces_session_and_rejects_writes` |
| RE-CONC-001 | concurrency | ingest writer 同时存在 | reader 不 claim、不阻塞超出 timeout、不改 state | `recent_entries_can_read_while_sqlite_writer_is_active` |
| RE-PERF-001 | query plan | 100k entries、3 active sources | 使用现有 source/time indexes；返回仍 bounded | `recent_entries_query_plan_uses_existing_indexes` |
| RE-COMPAT-001 | workspace | 全量既有测试 | 无回归 | `cargo test --workspace` |
| RE-DOC-001 | docs/map | CLI count、plan/code map | 无 drift | docs/map validation + diff review |

## 失败条件与 exit code

- 参数非法或缺 category：exit 2。
- config schema/driver mismatch：exit 78。
- DB 不存在、不可达、migration pending 或 query failure：exit 1。
- JSON error envelope 的 `command` 必须为 `recent-entries`，不得落为 `unknown` 或 `migrate`。

## 当前状态

`passing`

contract、实现与自动化 evidence 已闭环：USTC107 上 `fmt/check/clippy/test --workspace/release build` 及 100k-row resource smoke 已通过；GitHub Actions Docker-backed PostgreSQL gate 已验证 native `TIMESTAMPTZ` offset/fraction/boundary parity，并确认 command-grade read-only pool 的 `default_transaction_read_only=on`、写操作以 SQLSTATE `25006` 拒绝且数据不变。

## 相关文档

- Feed：[../../plan/01-feed.md](../../plan/01-feed.md)
- Storage：[../../plan/05-storage.md](../../plan/05-storage.md)
- CLI/runtime：[../../plan/09-cli-and-runtime.md](../../plan/09-cli-and-runtime.md)
- Non-goals：[../../plan/13-non-goals.md](../../plan/13-non-goals.md)
- 运维：[../../operations/cli-reference.md](../../operations/cli-reference.md)
