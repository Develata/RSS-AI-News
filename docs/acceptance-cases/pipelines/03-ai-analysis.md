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
- task_gen 遇身份行冲突时按已有行对账仍为 `Persisted` 的 article（pending/running → `AiPending`，
  succeeded/filtered → 成功派生态，permanent_failed → 保持并被游标越过）；batch=1 时新文章不饥饿
- publish 窗口份额（C9）：积压大于一批时窗口内文章仍得到任务并被领取；旧积压每 run 仍推进；
  batch=1 时 task_gen 与 claim 跨 run 交替（按持久记录的实际服务侧，窗口移动与重试都不打断）；fresh / backlog
  两侧不相交；窗口 0 = 全部 fresh
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
- `ai-run` 在 `[ai].enabled=false` 时直接返回 `ConfigError::AiRunWhileDisabled`（exit 78）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `task_gen_inserts_pending_and_advances_article_to_ai_pending` | `crates/runtime/tests/ai_run_tests.rs` | 任务生成 |
| `task_gen_skips_articles_already_advanced` | 同上 | 重入幂等 |
| `task_gen_only_scans_requested_category` | 同上 | category 隔离 |
| `task_gen_reconciles_conflicting_pending_row_and_reaches_newer_articles_with_batch_one` | 同上 | 冲突对账 + 不饥饿 |
| `task_gen_moves_past_permanently_failed_rows_within_one_pass` | 同上 | 游标越过失败行 |
| `task_gen_reconciles_finished_rows_to_the_post_ai_state` | 同上 | 成功派生态对账 |
| `reconcile_uses_another_identitys_success_when_this_one_was_filtered` | 同上 | R2-S3 双身份对账离开 persisted |
| `reconcile_with_another_low_score_success_reaches_ai_done` | 同上 | R2-S3 低分 → ai_done |
| `an_insert_error_on_a_short_page_stops_the_pass` | 同上 | R2-S4 短页出错后停止 |
| `an_insert_error_before_the_quota_is_met_stops_the_pass` | 同上 | R2-S4 名额用满前出错后停止 |
| `fresh_article_gets_a_task_despite_a_backlog_larger_than_one_batch` | 同上 | C9 fresh 不饥饿 |
| `fresh_share_prefers_articles_that_leave_the_window_first` | 同上 | C9 fresh 排序 |
| `older_backlog_keeps_advancing_while_fresh_inflow_continues` | 同上 | C9 旧积压仍推进 |
| `batch_size_one_task_gen_alternates_between_fresh_and_backlog` | 同上 | C9 batch=1 交替 |
| `a_stuck_fresh_row_does_not_block_batch_size_one` | 同上 | C9 卡住行不挡 |
| `window_zero_treats_every_article_as_fresh` | 同上 | C9 窗口 0 |
| `claim_gives_fresh_tasks_a_share_when_older_tasks_fill_the_queue` | 同上 | C9 claim 份额 |
| `batch_size_one_claims_alternate_between_fresh_and_older_tasks_across_runs` | 同上 | C9 claim 交替 |
| `batch_size_one_task_gen_alternation_survives_a_moving_window` | `crates/runtime/tests/ai_run_tests.rs` | R2-B1 task_gen 窗口移动后仍交替 |
| `batch_size_one_claim_alternation_survives_a_moving_window` | 同上 | R2-B1 claim 窗口移动后仍交替 |
| `batch_size_two_task_gen_backlog_slot_is_not_taken_by_fresh_articles` | 同上 | R2-B1 task_gen 两侧不相交 |
| `batch_size_two_claim_backlog_slot_is_not_taken_by_fresh_tasks` | 同上 | R2-B1 claim 两侧不相交 |
| `batch_size_one_claim_retries_count_as_served_turns` | 同上 | R2-B1 重试也算一轮 |
| `future_dated_articles_are_not_fresh_for_task_gen` | `crates/runtime/tests/ai_run_tests.rs` | R2-S1 窗口上界 = now（task_gen） |
| `future_dated_tasks_are_not_fresh_for_the_claim` | 同上 | R2-S1 窗口上界 = now（claim） |
| `equal_instants_in_different_offsets_tie_by_article_id` | `crates/storage/tests/ai_fresh_order_tests.rs` | R2-S2 按时刻排序、article id 打平 |
| `fresh_claim_ties_by_article_id_not_task_id` | 同上 | R2-S2 claim 按 article id 打平 |
| `claimed_rows_come_back_in_candidate_order` | 同上 | R2-S2 领取保序 |
| `a_sub_millisecond_instant_before_the_window_start_is_not_fresh` | 同上 | R2-S1 毫秒边界向内取整 |
| `pg_fresh_claim_orders_by_source_time_then_article_id` | `crates/storage/tests/article_ai_result_pg_tests.rs` | R2-S2 PG 排序与保序 |
| `a_failed_second_claim_side_leaves_the_first_side_unclaimed` | `crates/runtime/tests/ai_run_tests.rs` | R2-B2 整批同事务回滚 |
| `pg_fresh_share_lists_claims_and_signals_by_source_time` | `crates/storage/tests/article_ai_result_pg_tests.rs` | C9 PG SQL |
| `real_sqlite_extended_lock_error_returns_task_to_pending_with_budget_left` | `crates/runtime/tests/ai_run_tests.rs` | R2-B3 真实驱动扩展码 517 可重试 |
| `sqlite_extended_busy_code_from_the_driver_is_retryable` | `crates/storage/tests/sqlite_error_classification_tests.rs` | R2-B3 分类器（低 8 位） |
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
