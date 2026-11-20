# extract retryable 失败冷却，不再整轮终止

- 日期：2026-11-20
- 作者 / Agent：Claude Code（Opus）
- 分支：`fix/extract-retry-cooldown`（基于 `main` a047120）
- 当前 HEAD：见 `git log -1`
- 相关 commit：1424931、9856680、384897f（第一轮复审修复）、4b5f971（第二轮复审修复）、
  e66206b（SQLite lease 按时刻比较）、第三轮复审修复提交；随 v0.10.4 发布
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
  `FeedEntryRepository::claim_pending_fetch` 新增 `exclude_ids` 参数，
  `release_retryable_failure` 新增 `retry_at` 参数；`ExtractDeps` 新增 `clock`；
  `ExtractSummary.retryable_deferred` 语义改变；
  plan/02 §5.3、plan/05 §5.1 / §7.1、plan/08 §3、AC-P-02 已同步

## 关键变更

- release 在同一条 owner-guarded UPDATE 内写 `next_attempt_at`；backoff 10 / 20 / 40 / 80 min，上限 2 h。
- claim 按每次 claim 的时刻过滤冷却行（`LIMIT` 前）；排除本 run 已领取、未确认终态成功的 id，
  失败行（含 release 因 lease 被并发 run reclaim 而落空）不在同一 run 内重领。
- 截止点存为 UTC、向上取整到毫秒，claim 时刻向下取整到毫秒：冷却比较从不提前。SQLite 用
  `julianday()`（RFC3339 TEXT 小数位数可变，同一秒内字典序 ≠ 时刻序）。
- `run_completed` 事件在 claim 出错时也带完整计数与 `claim_error`。
- 删除 retryable-bail；循环到 claim 为空或 `max_batches` 为止。
- 预算、终态折叠、exhaustion sweep、过期 lease reclaim、永久失败、summary fallback 不变；backfill reset 清空冷却。

## 验证与验收

### 自动化验证

- `cargo fmt --all -- --check`：通过
- `cargo clippy --workspace --all-targets --jobs 1 -- -D warnings`：通过
- `cargo test --workspace`：1061 passed / 0 failed / 74 ignored；ignored 主要是需 docker 的 PG
  用例，另有手工执行的性能测试
- `cargo test -p rss-ai-news-storage --test feed_entry_pg_tests --test migrations_postgres_apply_tests --test release_compatibility_tests -- --include-ignored`：13 passed（含 PG 冷却 / 排除列表、0007 up / undo）
- `python3 -B tools/perf/query_plans.py`：exit 0，extract claim 仍走 `idx_feed_entries_state_lease_expires_at`
- 变异检查（每项都有指名的新测试失败）：claim 时刻冻结为 run 开始、去掉排除集合、冷却从
  策略链之前起算、release 不做毫秒取整、SQLite 冷却比较退回字典序
- SQLite 0007 down：以 `sqlite3` 3.54 手工执行 0001–0007 up 后 0007 down，列消失

### 未执行

- 其余 PG（docker）用例未跑；未在生产数据规模上验证。

### 独立复审

- Codex（gpt-6.2-astra，effort high）复审 `a047120..9856680`：2 个 BLOCKER（SQLite 字典序比较、
  lease reclaim 后同 run 重领）与 SHOULD_FIX 项已修复；`retryable_deferred` 在 release 冲突时置位
  这一点未采纳（冲突时重试同样推迟到之后的 run）。

- 第二轮 Codex 复审 `384897f`：2 个 BLOCKER（毫秒舍入可提前领取；固定 run-start cutoff 漏掉
  run 期间到期的工作）与 SHOULD_FIX（排除列表 O(N²)、`+15:00` 偏移不可解析、
  `retryable_deferred` 语义、采样时刻测试不够强）已在 4b5f971 修复。
- SQLite lease 比较原为字典序（基线缺陷），已在 e66206b 改为按时刻比较（另一会话完成）。
- 第三轮 Codex 复审 `384897f..e66206b`：
  - 采纳：旧数据中 `julianday()` 不可解析的 lease / 截止点改为回退文本比较（否则永久跳过）；
    亚毫秒测试补上能区分 claim 侧取整的边界，PG 改走 release 写入路径；retryable release 的
    `updated_at` 与截止点用同一时刻；文档中 SQLite reclaim 示例、误差上界（约 1.5 ms）与
    fresh-first 提案的窗口上界已更正。
  - 按计划处理：版本号由 v0.10.4 发版提交对齐。
  - 不适用：曾运行中间版本 `384897f` 的库——该版本从未推送或部署；"从不提前"针对本版本写入的截止点。
  - 记录不改：失败密集时排除列表的累计传输约 F(F−1)/2，见 plan/02 §5.3。

## 结果

- 可合并到本地；未推送、未部署、未回填 11-19 / 11-20。
- 回退：应用 0007 后，v0.10.3 二进制会拒绝未知迁移；回退前须先执行 0007 down。
- 已知限制：claim 仍按 `discovered_at ASC`，可领取旧积压大于 `batch_size × max_batches` 时新条目
  仍可能整轮排不上（测试 `oldest_first_backlog_still_starves_fresh_entries_under_cap`）。
- 同类模式未改：`flows/ai_run/mod.rs` 仍在 retryable 失败后停止 claim。
