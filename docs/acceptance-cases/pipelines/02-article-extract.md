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
- HTML 留档在策略调用**之前**完成（保证 replay-html 可重放真实输入）
- `--max-batches N` 限批生效；`--max-batches 0` 等于不限直到队列消尽

### 失败条件（failure path）

- 5xx → 状态保持 `PendingFetch`，lease 释放、可重试
- 4xx → state=`Failed`，不计入下一轮 claim
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
| `extract_writes_html_artifact_before_strategy` | 同上 | artifact 顺序 |
| `max_batches_caps_loop_and_reports_reached_flag` | 同上 | 限批生效 |
| `max_batches_zero_means_unlimited_until_queue_drained` | 同上 | 0 = 不限 |
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
