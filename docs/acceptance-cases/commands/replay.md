# AC-C-01: replay 子命令

## 功能描述

按 `raw_artifacts.id` / `(kind, key)` 读取留档原始字节，在内存中重做一次解析
（feed / html / ai 三种 kind），输出解析结果与（如有）与线上结果的 diff。

**只读、无副作用**：不写业务表、不写新 artifact、不动状态机。

面向场景：线上结果与当下代码不一致时，定位是输入变了还是解析逻辑变了。

## 验收标准

### 命中条件（success path）

- `replay --kind feed --id <N>` → 调用 `parse_feed(bytes, FeedKind::Rss)`，输出 entry_count + samples
- `replay --kind html --id <N>` → 按 `extractor.strategy_order` 走线上同一条 HTML 策略链（Readability），输出 title / body 长度；base URI 为 artifact key 对应 feed entry 的 `normalized_link`，无此 entry 时为占位符 `about:blank`，并在 `parsed.base_uri` 回显
- `replay --kind html --diff` → 经 feed entry 找到其文章（含 dedup entry），对比 content hash 与字数；无文章时 `{"found": false}`
- 只读分层：CLI → `ReplayFlow` → repository（`ReplayProjectionRepository`），CLI 不直接发 SQL
- `replay --kind ai --id <N>` → 调用 `parse_response`，输出 ParsedResponse + schema 校验结果
- `--id` 与 `--key` 二选一；都缺时 CLI 报错
- summary 既支持 pretty 输出也支持 JSON 输出

### 失败条件（failure path）

- artifact 找不到 → `CliError::ReplayArtifactNotFound`，exit 1
- artifact 命中但 `inline_body` 为空（文件后端） → `"file-backed artifacts not supported in W9c replay"`，exit 1（已知限制）
- 既无 `--id` 也无 `--key` → `"replay requires either --id or --key"`，exit 1；在加载配置 / 打开数据库**之前**报出，
  数据库不可用时也不被 `storage` 错误掩盖（R3-17）
- `--kind html` 且 `strategy_order` 只含 `summary_fallback`（无 HTML 策略）→ `REPLAY_HTML_NO_STRATEGY` 配置矛盾错误，exit 1，
  不读 artifact（线上该配置不解析 HTML，replay 不换用别的策略）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `args_parsing_parses_replay_with_kind_and_key` | `crates/cli/tests/args_parsing_tests.rs` | flag 解析 |
| `args_parsing_parses_replay_kind_id_conflicts` | 同上 | id / key 互斥 |
| `replay_summary_pretty_renders` | `crates/cli/tests/w9c_cli_tests.rs` | pretty 输出 |
| `replay_summary_serializes_parsed_payload` | 同上 | JSON 输出 |
| `replay_not_found_error_kind_is_specific` | 同上 | not-found 错误分类 |
| `raw_artifact_find_by_id_found` | `crates/storage/tests/w9c_storage_tests.rs` | repo find_by_id 命中 |
| `raw_artifact_find_by_id_missing` | 同上 | repo find_by_id 缺失 |
| `find_by_key_returns_inserted_row` | `crates/storage/tests/raw_artifact_tests.rs` | repo find_by_key |
| `find_by_key_returns_none_when_missing` | 同上 | repo find_by_key 缺失 |
| `html_replay_uses_the_feed_entry_link_as_base_uri` | `crates/runtime/src/flows/replay.rs` | base URI = entry link（相对链接 artifact） |
| `html_replay_without_a_feed_entry_uses_the_placeholder_base_uri` | 同上 | 占位符 base URI |
| `html_diff_finds_the_article_through_the_feed_entry` | 同上 | diff 经 entry（含 dedup）找文章 |
| `replay_reports_missing_and_file_backed_artifacts` | 同上 | 找不到 / 文件后端拒绝 |
| `an_artifact_of_another_kind_is_not_replayed` | 同上 | `--id` 命中他类 artifact 视为找不到 |
| `html_replay_renders_with_the_feed_entry_link_as_base_uri` | `crates/cli/src/commands/replay.rs` | 完整命令路径（装配 + Readability + JSON） |
| `replay_failures_keep_their_error_kinds` | 同上 | not-found / 缺 selector / storage 错误分类 |
| `html_replay_without_an_html_strategy_reports_the_contract_error` | `crates/runtime/src/flows/replay.rs` | 仅 summary_fallback → 契约错误，feed 不受影响 |
| `html_replay_runs_the_configured_chain_in_order` | 同上 | 策略链顺序与失败回退 |
| `html_replay_falls_back_to_the_feed_entry_title` | 同上 | 无 HTML 标题时用 entry `title_raw`，无 entry 用占位标题 |
| `html_replay_follows_the_configured_strategy_order` | `crates/cli/tests/replay_cmd_tests.rs` | 真实 CLI 装配，两种 strategy_order |
| `missing_selector_is_reported_before_the_database_is_opened` | 同上 | 缺 selector 先于打开数据库报错 |
| `sqlite_html_replay_source_reaches_entry_link_and_article` / `pg_…`（ignored） | `crates/storage/tests/replay_projection_tests.rs` | 双方言 projection |

## 当前状态

`partial`

已知限制：文件后端 artifact 不支持（W9c 范围内仅 inline_body 路径）。后续若启用 retention=`sampled`
或长 artifact 落盘，需扩展 replay 加文件读取分支。

## 相关文档

- 设计：[../../plan/10-replay-and-backfill.md](../../plan/10-replay-and-backfill.md) §replay
- raw_artifacts 留档：[../../plan/05-storage.md](../../plan/05-storage.md) §9 / [../../plan/02-extract.md](../../plan/02-extract.md) §artifact 顺序
- 错误模型：[../../plan/11-error-and-recovery.md](../../plan/11-error-and-recovery.md)
