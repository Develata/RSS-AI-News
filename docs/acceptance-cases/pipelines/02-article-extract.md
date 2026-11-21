# AC-P-02: 正文提取流水线

## 功能描述

从 `FeedEntry` 抓详情页 HTML，经 `ContentStrategy` 策略链（Readability → SummaryFallback）
抽取正文，写入 `articles`；命中**第三层 content_hash 去重**则跳过；写入前 HTML 原文按
retention policy 留档为 `raw_artifacts.kind='html_payload'`。

面向场景：`ingest` 之后的 `extract` 阶段，或 `run` 子命令的第二段。

## 验收标准

### 命中条件（success path）

- 抓取成功 + Readability 抽出正文 → 写 `articles`，state=`Persisted`
- 非 UTF-8 页面（BOM / `Content-Type` charset / `<meta>` 声明）先解码再提取；未声明且非 UTF-8 → `ParseFailed`
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
  `max_batches`；本 run 失败过的行不会被本 run 再次领取（含 lease 被并发 run reclaim 导致
  release 落空的情形），而 run 期间才到期、本 run 未尝试过的行可以被领取
- 只剩冷却中的行 → claim 为空，run 立即结束（不空转）
- 4xx 且 summary 为空 → state=`Failed`，不计入下一轮 claim
- article 写入或摘要回读遇到可重试存储错误 → 走 retryable 冷却 release（预算内回 `PendingFetch`，
  最后一次尝试折叠 `Failed`），`last_error_kind` 为真实存储错误；不可重试存储错误 → `Failed`
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
| `transient_article_insert_failure_is_retried_with_cooldown` | `crates/runtime/tests/extract_storage_failure_tests.rs` | 瞬时写入失败回队 + 冷却 |
| `transient_article_insert_failure_on_last_attempt_folds_to_failed` | 同上 | 最后一次尝试折叠 failed |
| `non_retryable_article_insert_failure_is_permanent` | 同上 | 不可重试存储错误终态 |
| `transient_summary_read_failure_after_403_is_not_terminal` | 同上 | 摘要回读瞬时失败不终态（抓取 403） |
| `transient_summary_read_failure_after_parse_failure_is_not_terminal` | 同上 | 摘要回读瞬时失败不终态（解析失败） |
| `permanent_release_that_misses_reports_lease_lost_without_event` | `crates/runtime/tests/extract_tests.rs` | 永久 release 落空报 LeaseLost、无事件 |
| `success_release_that_misses_reports_lease_lost` | 同上 | 成功 release 落空不报 Persisted |
| `failed_permanent_release_reports_release_failed` | `crates/runtime/tests/extract_storage_failure_tests.rs` | release 报错为 ReleaseFailed + storage_failed |
| `extract_storage_failures_fail_the_run_but_lost_leases_do_not` | `crates/cli/src/commands/ingest.rs`（单元测试） | extract 存储失败进入退出码 |
| `extract_writes_html_artifact_before_strategy` | 同上 | artifact 顺序 |
| `max_batches_caps_loop_and_reports_reached_flag` | 同上 | 限批生效 |
| `max_batches_zero_means_unlimited_until_queue_drained` | 同上 | 0 = 不限 |
| `timeout_on_first_entry_does_not_block_later_healthy_entry` | 同上 | 超时不阻断后续条目；同 run 不重领 |
| `cooling_entry_is_skipped_until_deadline_then_retried_by_later_run` | 同上 | 截止前跳过、之后重试 |
| `queue_of_only_cooling_entries_exits_without_spinning` | 同上 | 只剩冷却行不空转 |
| `max_batches_still_caps_a_run_with_retryable_failures` | 同上 | 限批在 retryable 下仍生效 |
| `oldest_first_backlog_still_starves_fresh_entries_under_cap` | 同上 | 已知限制：旧积压可挤占新条目 |
| `entry_whose_lease_was_reclaimed_is_not_claimed_again_in_the_same_run` | `crates/runtime/tests/extract_tests.rs` | release 落空也不同 run 重领 |
| `entry_due_mid_run_is_claimed_but_entry_failed_this_run_is_not` | 同上 | 到期按每次 claim 判断；失败行同 run 排除 |
| `cooldown_is_measured_from_the_release_not_from_the_fetch` | 同上 | 冷却从 release 起算 |
| `release_deadline_is_never_early_and_always_parseable` | `crates/storage/tests/feed_entry_retry_cooldown_tests.rs` | 亚毫秒与 `+15:00` 偏移 |
| `retryable_release_sets_deadline_and_claim_honours_it` | `crates/storage/tests/feed_entry_retry_cooldown_tests.rs` | cutoff 规则（SQLite） |
| `cooldown_compares_instants_not_text` | 同上 | 亚秒 / 时区偏移按时刻比较 |
| `excluded_ids_are_not_claimed` | 同上 | 本 run 已领取 id 排除 |
| `pg_retry_cooldown_is_honoured_by_claim` | `crates/storage/tests/feed_entry_pg_tests.rs` | cutoff 规则（PG，需 docker） |
| `readability_extracts_simple_article` | `crates/extractor/tests/strategy_tests.rs` | 策略层 happy |
| `readability_returns_content_too_short_for_short_article` | 同上 | 太短拒收 |
| `readability_returns_parse_failed_for_no_content` | 同上 | 无正文 |
| `gbk_page_declared_by_header_is_decoded_before_readability` | `crates/runtime/tests/extract_tests.rs` | 先解码再提取，artifact 保留原始字节 |
| `gbk_page_declared_only_by_the_content_type_header_decodes` | `crates/extractor/tests/charset_tests.rs` | 仅响应头声明 GBK |
| `gbk_page_declared_only_by_meta_decodes` | 同上 | 仅 `<meta>` 声明 |
| `http_equiv_big5_and_shift_jis_pages_decode` | 同上 | Big5 / Shift_JIS |
| `undeclared_invalid_utf8_still_fails_without_guessing` | 同上 | 未声明的非 UTF-8 仍 `ParseFailed` |
| `declared_charset_with_malformed_bytes_is_rejected_not_replaced` | 同上 | 严格解码，不插替换字符 |
| `readability_grades_a_2340_char_chinese_article_high` | 同上 | 中文长文按 CJK 计词为 High |
| `content_too_short_reports_characters_not_bytes` | 同上 | `ContentTooShort.chars` 为字符数 |
| `summary_fallback_counts_cjk_words` | 同上 | fallback 字数同一规则 |
| `cjk_boundaries_around_medium_and_high` | `crates/extractor/src/strategy.rs`（单元测试） | 199/200、599/600 汉字边界 |
| `mixed_text_counts_latin_runs_and_halves_cjk` | 同上 | 中英混排、假名、谚文 |
| `non_cjk_text_counts_like_split_whitespace` | 同上 | 非 CJK 文本计数不变 |
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
