# AC-C-02: backfill 子命令

## 功能描述

按时间窗 + 目标段重做线上业务：
- `--target extract`：把窗内 `Failed` / `FallbackPersisted` 的 `FeedEntry` 重置回 `PendingFetch`，
  等待下一次 ingest / extract 重做
- `--target ai`：新建 `prompt_versions` 行（active 升级），对窗内 article 批量插入新 prompt_version
  的 `Pending` ai_result（不动旧 ai_result）

与 `reindex` 的边界：`backfill` 重做业务，**不**算法升级；`reindex` 升级算法 → 重算字段。

面向场景：模型/prompt 升级要做平行实验、Bug 修复后回补失败窗口。

## 验收标准

### 命中条件（success path）

- `--target extract` + 时间窗 → 重置窗内 Failed/FallbackPersisted entry；不动 Persisted 行
- `--target extract` + 无时间窗 → 重置全部 Failed
- examined 计数与窗内实际命中数一致
- `--target ai` → 写入新 `prompt_versions` 行；窗内 `Persisted` / `AiDone` article 各自插入新 prompt_version 的 Pending ai_result，**不改 article 状态**
- `(article_id, prompt_version_id)` 唯一冲突 → 不重复插入，conflict 计数累计
- 分页（按 article id）覆盖所有窗内 article
- `--prompt-version-tag` 未指定时 fallback 为 `backfill-<unix-ts>`
- summary（pretty/JSON）输出 prompt_version_id / tag / model_id / inserted / conflict 计数

### 失败条件（failure path）

- DB 写失败透传为 `RuntimeError::Storage`，exit 3
- 无候选时仍返回成功（examined=reset=0），exit 0
- 与 `--target ai` 同时使用错的 `--model` 不被 args 阻拦（args 仅解析，业务由 prompt_versions 写入校验）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `args_parsing_parses_backfill_with_target_extract` | `crates/cli/tests/args_parsing_tests.rs` | flag 解析 |
| `args_parsing_backfill_accepts_version_override_fields` | 同上 | prompt 覆盖字段 |
| `args_parsing_backfill_version_override_fields_default_to_none` | 同上 | 默认值 |
| `backfill_extract_resets_failed_entries_in_window` | `crates/runtime/tests/w9c_runtime_tests.rs` | extract 窗内重置 |
| `backfill_extract_does_not_touch_persisted_entries` | 同上 | persisted 不动 |
| `backfill_extract_with_no_window_resets_all_failed` | 同上 | 无窗口 |
| `backfill_extract_examined_counts_window_intersection` | 同上 | 计数正确 |
| `backfill_ai_creates_new_prompt_version_row` | 同上 | 新 prompt_version |
| `backfill_ai_inserts_pending_for_persisted_without_state_change` | 同上 | persisted 平行任务 |
| `backfill_ai_inserts_pending_for_ai_done_without_state_change` | 同上 | ai_done 平行任务 |
| `backfill_ai_skips_already_existing_tuple` | 同上 | tuple 唯一 |
| `backfill_ai_pagination_covers_all_articles` | 同上 | 分页全覆盖 |
| `reset_failed_resets_all_failed` | `crates/storage/tests/w9c_storage_tests.rs` | repo 层重置 |
| `reset_failed_honors_window` | 同上 | repo 窗口 |
| `reset_failed_ignores_non_failed` | 同上 | repo 不动其它状态 |
| `article_backfill_lists_all_non_retired` | 同上 | repo article 列表 |
| `article_backfill_honors_date_from` / `_date_to_and_after_id` | 同上 | repo 分页 |
| `backfill_summary_pretty_renders` | `crates/cli/tests/w9c_cli_tests.rs` | summary pretty |
| `backfill_summary_serializes_inserted_count` | 同上 | summary JSON |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/10-replay-and-backfill.md](../../plan/10-replay-and-backfill.md) §backfill
- 状态机：[../../plan/08-state-machines.md](../../plan/08-state-machines.md)
- AI prompt versioning：[../../plan/03-ai.md](../../plan/03-ai.md)
