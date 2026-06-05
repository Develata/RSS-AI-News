# AC-C-07: rebuild-report 子命令

## 功能描述

按 `publish_records.id` 用**当前**模板 + 冻结 snapshot 重新渲染 Markdown，写入本地（可选 GitHub）。
**不**修改 `publish_records` 行；snapshot 保持冻结。

字节相等保证：
- 模板未变 + 同 generated_at → 重建结果与原文件 byte-for-byte 一致
- 模板已变 → 字节差异即模板改动的影响范围，作模板修复的回归依据

面向场景：模板修复后重发指定历史报告；本地手动重渲染验证。

## 验收标准

### 命中条件（success path）

- 模板 + render config 与原 publish 一致时 → 重建结果与原文件字节相等
- `--generated-at` 未指定 → fallback 到 `publish_records.rendered_at`，仍字节相等
- summary 输出 publish_record_id + 写入字节数 + 路径
- `--local-only` 仅写本地

### 失败条件（failure path）

- `publish_record_id` 不存在 → `CliError::PublishRecordNotFound`，exit 1
- 远端 422 lost-update 重试达上限 → exit 1，本地文件仍已写
- 远端 401 → `GithubAuthFailed`，exit 1

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `args_parsing_parses_rebuild_report_with_publish_id` | `crates/cli/tests/args_parsing_tests.rs` | args 解析 |
| `rebuild_returns_byte_equal_markdown_to_original_render` | `crates/runtime/tests/rebuild_report_tests.rs` | 字节相等 |
| `rebuild_without_generated_at_override_falls_back_to_record_rendered_at_and_matches_original` | 同上 | fallback 字节相等 |
| `rebuild_returns_error_when_publish_record_id_not_found` | 同上 | 缺失报错 |
| `rebuild_returns_byte_equal_markdown_when_render_config_matches` | `crates/report/tests/rebuild_tests.rs` | report 层字节相等 |
| `rebuild_returns_error_when_publish_record_missing` | 同上 | report 层缺失 |
| `rebuild_report_summary_pretty_renders` | `crates/cli/tests/w9c_cli_tests.rs` | summary pretty |
| `rebuild_report_summary_serializes_bytes` | 同上 | summary JSON |
| `publish_record_not_found_error_kind_is_specific` | 同上 | 错误分类 |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/10-replay-and-backfill.md](../../plan/10-replay-and-backfill.md) §rebuild-report
- 模板：[../../plan/04-publish.md](../../plan/04-publish.md) §模板
- 决策：`../../adr/0003-publish-snapshot-immutable.md`
