# AC-P-02: 正文提取流水线

## 功能描述

从 `FeedEntry` 抓详情页 HTML，经 `ContentStrategy` 策略链（Readability → SummaryFallback）
抽取正文，写入 `articles`；命中**第三层 content_hash 去重**则跳过；写入前 HTML 原文按
retention policy 留档为 `raw_artifacts.kind='html_payload'`。

面向场景：`ingest` 之后的 `extract` 阶段，或 `run` 子命令的第二段。

## 验收标准

### 命中条件（success path）

- 抓取成功 + Readability 抽出正文 → 写 `articles`，state=`Persisted`
- 同 content_hash 已存在 → 跳过插入，FeedEntry → `DedupSkipped`（第三层去重）
- Readability 失败但 summary 非空 → SummaryFallback 兜底写入，state=`FallbackPersisted`
- 抓取**永久失败**（4xx/too_large/invalid_url）但 summary 非空 → SummaryFallback
  兜底写入（无 html artifact），state=`FallbackPersisted`（W18，[../../plan/17-extract-fetch-fallback.md](../../plan/17-extract-fetch-fallback.md)）
- HTML 留档在策略调用**之前**完成（保证 replay-html 可重放真实输入）
- `--max-batches N` 限批生效；`--max-batches 0` 等于不限直到队列消尽

### 失败条件（failure path）

- 5xx / 超时且重试预算未尽 → 状态保持 `PendingFetch`，lease 释放、写入 `next_attempt_at`
  冷却；冷却截止前不被 claim，之后的 run 可重试（**即使 summary 可用也不提前降级**，W18）
- 5xx / 超时发生在预算内最后一次尝试（`attempt_count == max_attempts`）→ 直接转 `Failed`
- retryable 失败不终止本 run：同 run 内继续领取其他可领取行，直到队列为空或达到
  `max_batches`；每行每 run 至多被领取一次（含 lease 被并发 run reclaim 导致 release 落空的情形）
- 只剩冷却中的行 → claim 为空，run 立即结束（不空转）
- 4xx 且 summary 为空 → state=`Failed`，不计入下一轮 claim
- 策略链 + fallback 都失败 → state=`Failed`，artifact 仍保留（按 policy）
- content_hash 在 articles 表已存在但 link 不同 → repo 层报 `ArticleConflict`（用 `dedup_tests::articles_content_hash_duplicate_is_conflict` 兜底）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `extract_persists_new_article_on_success` | `crates/runtime/tests/extract_tests.rs` | Readability happy |
| `extract_dedup_skipped_when_content_hash_matches_existing_article` | 同上 | 第三层去重 |
| `extract_falls_back_to_summary_when_strategy_chain_fails` | 同上 | SummaryFallback 兜底 |
| `extract_marks_failed_when_strategy_and_fallback_both_fail` | 同上 | 全失败路径 |
| `extract_releases_retryable_on_5xx` | 同上 | 5xx 可重试 |
| `extract_marks_failed_on_4xx` | 同上 | 4xx 永久失败 |
| `extract_fetch_403_with_summary_persists_fallback` | 同上 | W18 抓取失败兜底 |
| `extract_fetch_403_without_summary_fails_permanent` | 同上 | W18 无摘要仍 Failed |
| `extract_fetch_5xx_with_summary_stays_retryable` | 同上 | W18 可重试不降级 |
| `extract_writes_html_artifact_before_strategy` | 同上 | artifact 顺序 |
| `max_batches_caps_loop_and_reports_reached_flag` | 同上 | 限批生效 |
| `max_batches_zero_means_unlimited_until_queue_drained` | 同上 | 0 = 不限 |
| `timeout_on_first_entry_does_not_block_later_healthy_entry` | 同上 | 超时不阻断后续条目；同 run 不重领 |
| `retryable_release_records_bounded_cooldown_after_run_start` | 同上 | 冷却截止点 |
| `cooling_entry_is_skipped_until_deadline_then_retried_by_later_run` | 同上 | 截止前跳过、之后重试 |
| `queue_of_only_cooling_entries_exits_without_spinning` | 同上 | 只剩冷却行不空转 |
| `max_batches_still_caps_a_run_with_retryable_failures` | 同上 | 限批在 retryable 下仍生效 |
| `oldest_first_backlog_still_starves_fresh_entries_under_cap` | 同上 | 已知限制：旧积压可挤占新条目 |
| `entry_whose_lease_was_reclaimed_is_not_claimed_again_in_the_same_run` | `crates/runtime/tests/extract_tests.rs` | release 落空也不同 run 重领 |
| `retryable_release_sets_deadline_and_claim_honours_it` | `crates/storage/tests/feed_entry_retry_cooldown_tests.rs` | cutoff 规则（SQLite） |
| `cooldown_compares_instants_not_text` | 同上 | 亚秒 / 时区偏移按时刻比较 |
| `excluded_ids_are_not_claimed` | 同上 | 本 run 已领取 id 排除 |
| `pg_retry_cooldown_is_honoured_by_claim` | `crates/storage/tests/feed_entry_pg_tests.rs` | cutoff 规则（PG，需 docker） |
| `readability_extracts_simple_article` | `crates/extractor/tests/strategy_tests.rs` | 策略层 happy |
| `readability_returns_content_too_short_for_short_article` | 同上 | 太短拒收 |
| `readability_returns_parse_failed_for_no_content` | 同上 | 无正文 |
| `summary_fallback_uses_summary_raw_when_present` | 同上 | fallback 策略 |
| `summary_fallback_returns_none_when_summary_empty_or_only_html_tags` | 同上 | fallback 边界 |
| `articles_content_hash_duplicate_is_conflict` | `crates/storage/tests/dedup_tests.rs` | repo 层冲突 |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/02-extract.md](../../plan/02-extract.md)
- 状态机：[../../plan/08-state-machines.md](../../plan/08-state-machines.md)（FeedEntryState / ArticleState）
- 回放：[../../plan/10-replay-and-backfill.md](../../plan/10-replay-and-backfill.md) §replay-html
- 留档：[../../plan/05-storage.md](../../plan/05-storage.md) §raw_artifacts
