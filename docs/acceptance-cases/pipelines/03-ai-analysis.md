# AC-P-03: AI 分析流水线

## 功能描述

为 `articles.state = Persisted` 的文章生成 AI 任务（`ArticleAiResult.Pending`），
按 lease 并发调 OpenAI 兼容接口，解析 JSON 响应，按 `keep` / `score` 推动 `Article` 与
`AiResult` 状态机；命中 `[publish].min_importance_score` 进入候选，否则 `PublishSkipped`。
AI-off 直通模式跳过整段。

面向场景：`ai-run` 子命令，或 `run` 子命令的第三段。

## 验收标准

### 命中条件（success path）

- 任务生成：`Persisted` article 插入 `(article_id, prompt_version_id)` 唯一的 ai_result Pending 行；
  article 推进到 `AiPending`
- task_gen 跳过已推进的 article（重入幂等）
- task_gen 仅扫描 `--category` 指定分类（其它分类不动）
- AI 成功 + 高分 → article → `ReadyForPublish`、ai_result → `Succeeded`
- AI 成功 + 低分 → article → `AiDone`（不入发布候选）
- AI 返回 `keep=false` → article → `PublishSkipped`、ai_result → `Filtered`
- ai_raw_response artifact 在 lease release **之前**写入（保证 replay-ai 可回放）
- 进程仅 claim 自己 category 的任务
- AI-off 直通：`[ai].enabled=false` 时 `Persisted` article 直接进入候选

### 失败条件（failure path）

- HTTP 5xx → ai_result release 为可重试，state 保持 Pending，attempt+1
- 解析失败（缺字段 / 越界 / 非 JSON）→ ai_result → `PermanentFailed`（不可重试）
- 解析阶段对 `score` 越界、`keep=true` 缺 `summary` 等做强校验，错误分类详见 [../../plan/03-ai.md](../../plan/03-ai.md)
- `ai-run` 在 `[ai].enabled=false` 时直接返回 `ConfigError::AiRunWhileDisabled`（exit 2）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `task_gen_inserts_pending_and_advances_article_to_ai_pending` | `crates/runtime/tests/ai_run_tests.rs` | 任务生成 |
| `task_gen_skips_articles_already_advanced` | 同上 | 重入幂等 |
| `task_gen_only_scans_requested_category` | 同上 | category 隔离 |
| `process_succeeds_high_score_advances_article_to_ready_for_publish` | 同上 | 高分 happy |
| `process_succeeds_low_score_advances_article_to_ai_done` | 同上 | 低分跳过候选 |
| `process_filtered_advances_article_to_publish_skipped` | 同上 | keep=false |
| `process_writes_ai_raw_response_artifact_before_release` | 同上 | release 后 artifact 已关联（事后验证；严格顺序保证由实现侧承担） |
| `process_only_claims_requested_category` | 同上 | claim 隔离 |
| `process_releases_retryable_on_5xx_error` | 同上 | 5xx 可重试 |
| `process_releases_permanent_on_invalid_json` | 同上 | 永久失败 |
| `parse_keep_true_returns_ai_output` | `crates/ai/tests/parser_tests.rs` | 解析 happy |
| `parse_keep_false_returns_filtered_output` | 同上 | filtered |
| `parse_extracts_json_from_text_with_prefix_and_suffix` | 同上 | 鲁棒 JSON 提取 |
| `parse_returns_missing_field_when_keep_true_without_summary` | 同上 | 字段校验 |
| `parse_returns_invalid_field_value_when_score_out_of_range` | 同上 | score 越界 |
| `parse_returns_invalid_json_for_garbage_input` | 同上 | 垃圾输入 |
| `render_truncates_body_to_max_chars_with_ellipsis` | `crates/ai/tests/prompt_tests.rs` | prompt 截断 |
| `render_does_not_break_utf8_at_boundary` | 同上 | UTF-8 边界 |
| `render_does_not_re_substitute_placeholders_inside_replaced_content` | 同上 | placeholder 不二次替换 |
| `ai_result_unique_tuple_duplicate_returns_none_via_repo` | `crates/storage/tests/dedup_tests.rs` | `(article_id, prompt_version_id)` 唯一 |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/03-ai.md](../../plan/03-ai.md)
- 状态机：[../../plan/08-state-machines.md](../../plan/08-state-machines.md)（ArticleState / AiResultState）
- 回放：[../../plan/10-replay-and-backfill.md](../../plan/10-replay-and-backfill.md) §replay-ai / §backfill-ai
- 配置：[../../plan/06-config.md](../../plan/06-config.md) §[ai] × include_unscored 真值表
