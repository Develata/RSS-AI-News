# AC-C-01: replay 子命令

## 功能描述

按 `raw_artifacts.id` / `(kind, key)` 读取留档原始字节，在内存中重做一次解析
（feed / html / ai 三种 kind），输出解析结果与（如有）与线上结果的 diff。

**只读、无副作用**：不写业务表、不写新 artifact、不动状态机。

面向场景：线上结果与当下代码不一致时，定位是输入变了还是解析逻辑变了。

## 验收标准

### 命中条件（success path）

- `replay --kind feed --id <N>` → 调用 `parse_feed(bytes, FeedKind::Rss)`，输出 entry_count + samples
- `replay --kind html --id <N>` → 调用 Readability 策略，输出 title / body 长度
- `replay --kind ai --id <N>` → 调用 `parse_response`，输出 ParsedResponse + schema 校验结果
- `--id` 与 `--key` 二选一；都缺时 CLI 报错
- summary 既支持 pretty 输出也支持 JSON 输出

### 失败条件（failure path）

- artifact 找不到 → `CliError::ReplayArtifactNotFound`，exit 3
- artifact 命中但 `inline_body` 为空（文件后端） → `"file-backed artifacts not supported in W9c replay"`，exit 3（已知限制）
- 既无 `--id` 也无 `--key` → `"replay requires either --id or --key"`

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

## 当前状态

`partial`

已知限制：文件后端 artifact 不支持（W9c 范围内仅 inline_body 路径）。后续若启用 retention=`sampled`
或长 artifact 落盘，需扩展 replay 加文件读取分支。

## 相关文档

- 设计：[../../plan/10-replay-and-backfill.md](../../plan/10-replay-and-backfill.md) §replay
- raw_artifacts 留档：[../../plan/05-storage.md](../../plan/05-storage.md) §9 / [../../plan/02-extract.md](../../plan/02-extract.md) §artifact 顺序
- 错误模型：[../../plan/11-error-and-recovery.md](../../plan/11-error-and-recovery.md)
