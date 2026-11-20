# extract retryable 失败冷却，不再整轮终止

- 日期：2026-11-20
- 作者 / Agent：Claude Code（Opus）
- 分支：`fix/extract-retry-cooldown`（基于 `main` a047120）
- 当前 HEAD：见 `git log -1`
- 相关 commit：1424931、9856680，以及 Codex 复审后的修复提交
- 相关 tag / release：N/A（未发布、未部署）
- 状态：`validated`（仅本地）

## 工作摘要

v0.10.3 中 extract 一旦某批出现 retryable 失败就整轮退出（"retryable failures in batch;
deferring re-claim to next run"）。2026-11-20 18:20 / 18:30（北京时间）同一篇 Solidot
（feed_entry_id 1803140）超时，两轮都只跑了一批。现在 retryable 失败写入冷却截止点
`next_attempt_at`，run 继续领取其他可领取行。

## 影响范围

- crate / 模块：`crates/storage/src/repo/feed_entry{,_sql,_impl}.rs`、`crates/runtime/src/flows/extract.rs`
- 真相源对象：FeedEntry（新列 `next_attempt_at`；claim / retryable release / reset SQL）
- 额外影响：migration `0007_feed_entry_retry_cooldown`（两方言，已 pin）；
  `FeedEntryRepository::claim_pending_fetch` 新增 `retry_cutoff` 与 `exclude_ids` 参数，
  `release_retryable_failure` 新增 `retry_at` 参数；`ExtractSummary.retryable_deferred` 语义改变；
  plan/02 §5.3、plan/05 §5.1 / §7.1、plan/08 §3、AC-P-02 已同步

## 关键变更

- release 在同一条 owner-guarded UPDATE 内写 `next_attempt_at`；backoff 10 / 20 / 40 / 80 min，上限 2 h。
- claim 以 run 开始时刻为 cutoff 过滤冷却行（`LIMIT` 前），本 run 释放的行本 run 内不可再领；
  另排除本 run 已领取过的 id，release 因 lease 被并发 run reclaim 而落空时也不会重领。
- SQLite 冷却比较用 `julianday()`：RFC3339 TEXT 小数位数可变，同一秒内字典序 ≠ 时刻序。
- `run_completed` 事件在 claim 出错时也带完整计数与 `claim_error`。
- 删除 retryable-bail；循环到 claim 为空或 `max_batches` 为止。
- 预算、终态折叠、exhaustion sweep、过期 lease reclaim、永久失败、summary fallback 不变；backfill reset 清空冷却。

## 验证与验收

### 自动化验证

- `cargo fmt --all -- --check`：通过
- `cargo clippy --workspace --all-targets --jobs 1 -- -D warnings`：通过
- `cargo test --workspace`：1058 passed / 0 failed / 74 ignored；ignored 主要是需 docker 的 PG
  用例，另有手工执行的性能测试
- `cargo test -p rss-ai-news-storage --test feed_entry_pg_tests --test migrations_postgres_apply_tests --test release_compatibility_tests -- --include-ignored`：13 passed（含 PG 冷却 / 排除列表、0007 up / undo）
- `python3 -B tools/perf/query_plans.py`：exit 0，extract claim 仍走 `idx_feed_entries_state_lease_expires_at`
- 变异检查：cutoff 改为 per-claim `now`、恢复 retryable-bail、去掉 `claimed_ids` 排除、SQLite
  冷却比较退回字典序，均有新测试失败
- SQLite 0007 down：以 `sqlite3` 3.54 手工执行 0001–0007 up 后 0007 down，列消失

### 未执行

- 其余 PG（docker）用例未跑；未在生产数据规模上验证。

### 独立复审

- Codex（gpt-6.2-astra，effort high）复审 `a047120..9856680`：2 个 BLOCKER（SQLite 字典序比较、
  lease reclaim 后同 run 重领）与 SHOULD_FIX 项已修复；`retryable_deferred` 在 release 冲突时置位
  这一点未采纳（冲突时重试同样推迟到之后的 run）。

## 结果

- 可合并到本地；未推送、未部署、未回填 11-19 / 11-20。
- 回退：应用 0007 后，v0.10.3 二进制会拒绝未知迁移；回退前须先执行 0007 down。
- 已知限制：claim 仍按 `discovered_at ASC`，可领取旧积压大于 `batch_size × max_batches` 时新条目
  仍可能整轮排不上（测试 `oldest_first_backlog_still_starves_fresh_entries_under_cap`）。
- 同类模式未改：`flows/ai_run/mod.rs` 仍在 retryable 失败后停止 claim。
