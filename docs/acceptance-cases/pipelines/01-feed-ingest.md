# AC-P-01: Feed Ingest 流水线

## 功能描述

按 `categories/*.toml` 配置的 `[[sources]]` 抓取 RSS / Atom / JSON Feed / RSSHub feed，
经条件请求（`If-Modified-Since` / `If-None-Match`）后解析为 `FeedEntry`，
经一层 link 去重、二层 uid 去重后入库；失败 / 5xx 进入重试预算。

面向场景：定时（外挂 cron）触发 `ingest` 子命令，或 `run` 子命令的第一段。

## 验收标准

### 命中条件（success path）

- 单 source 200 + 全新 entries → 全量插入 `feed_entries`，state=`PendingFetch`
- 单 source 304 Not Modified → 不插入任何 entry，feed_source `last_etag` / `last_modified` 不变
- link_hash 在同 source 内 / 跨 source 命中已存在 entry → 去重跳过，发 `entry_dedup_skipped` 事件
- uid 命中已存在 entry → 同上
- 多 source 并发受 `[http].concurrent_feeds` 限制
- `feed_sources` 行在 ingest 起手会从当前 config 同步 enabled / kind / priority（已存在 source 也会被更新）
- bootstrap：首次 ingest 会把 active `config` kind rule_version_id 写入 `feed_sources.config_rule_version_id`

### 失败条件（failure path）

- 单 source 5xx → state=`Failed`，发 `source_fetch_failed` 事件，受 `[retry].feed_entry_max_attempts` 限制
- 解析失败 → artifact 保留（按 retention policy），entry / source 标记失败
- ingest 整体永不静默吞错；任何业务表写入失败必须经 `RuntimeError` 上抛（详见 [../../plan/11-error-and-recovery.md](../../plan/11-error-and-recovery.md)）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `single_source_200_inserts_all_entries` | `crates/runtime/tests/ingest_tests.rs` | 200 happy path |
| `single_source_304_marks_not_modified_no_entries` | 同上 | 304 短路 |
| `existing_source_is_synced_from_current_config_before_fetch` | 同上 | source 配置同步 |
| `single_source_5xx_marks_failed_writes_event` | 同上 | 5xx 失败路径 |
| `link_hash_dup_skipped_aggregated_event` | 同上 | 一层 link 去重 |
| `uid_dup_skipped_aggregated_event` | 同上 | 二层 uid 去重 |
| `parse_failure_keeps_artifact_marks_failed` | 同上 | 解析失败 + artifact |
| `multi_source_concurrent_within_limit` | 同上 | 并发上限 |
| `ingest_bootstrap_writes_config_kind_id_into_feed_sources_config_version` | 同上 | bootstrap rule 绑定 |
| `ingest_cmd_with_mock_feed_succeeds` | `crates/cli/tests/ingest_cmd_tests.rs` | CLI 入口 e2e |
| `ingest_cmd_with_failing_feed_records_error` | 同上 | CLI 失败路径 |
| `feed_entry_uid_unique_duplicate_returns_none` | `crates/storage/tests/dedup_tests.rs` | repo 层 uid 唯一 |
| `feed_entry_link_hash_lookup_distinguishes_hit_and_miss` | 同上 | repo 层 link_hash |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/01-feed.md](../../plan/01-feed.md)
- 状态机：[../../plan/08-state-machines.md](../../plan/08-state-machines.md)（FeedEntryState）
- 配置：[../../plan/06-config.md](../../plan/06-config.md)（`[[sources]]` schema）
- 决策（建设中）：`../../adr/0007-rsshub-secret-runtime-expansion.md`
