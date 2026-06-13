# 17 — extract 抓取期永久失败的摘要兜底（W18）

状态：已确认（2026-06-12，用户拍板）。

## 1. 问题

生产 PG 数据（2026-06-12，v0.5.0）：`feed_entries` 失败总量 4912，其中
`http_4xx` 3312 条（66%），高度集中在付费墙/反爬源（openai-news 988、
nyt-world 553、ft-world 484、economist-international 304…），且持续产生。

根因是 extract 流程的**不对称**：

| 失败位置 | 现状 | 结果 |
|---|---|---|
| 解析链失败（抓到 HTML 但策略全败） | 走 `summary_fallback` 兜底 | 摘要可用 → `fallback_persisted` |
| **抓取失败（403/404/too_large）** | **直接 `release_extract_error`** | 永久错误 → `failed`，**摘要被无视** |

付费墙站的 feed 普遍自带摘要（RSSHub `?mode=fulltext` 路由甚至近全文），
这些条目本可以摘要成文供 AI 评分出报告，却整条作废。全局 32K 条目中
80% 已经走 `fallback_persisted`，证明摘要级内容是本系统的主流形态——
该缺口是管道里唯一的成规模失血点。

## 2. 方案

`runtime/src/flows/extract.rs::process_entry` 抓取错误分支加一个对称判定：

- `error.is_retryable()`（http_timeout / http_5xx / connection_failed）→
  **维持现状**：`release_extract_error` 走 W15 重试预算路径，不消费摘要
  （下次重试可能抓到全文，提前降级是损失）。
- 永久性错误（http_4xx / too_large / invalid_url）→ 先试
  `summary_fallback(&fetch_task)`：
  - `Some(fallback)` → `persist_fallback(..., html_artifact_id=None, ...)`，
    条目转 `fallback_persisted`，与解析链失败分支完全同构；
  - `None`（feed 无摘要或剥 HTML 后为空）→ 维持现状转 `failed`。

不新增状态、不新增配置、不改 trait 签名；`html_artifact_id=None` 表达
"本条无 HTML 原文 artifact"（抓取未成功，无可留档之物）。

## 3. 不变量

- **重试语义零变化**：可重试错误的预算折叠（W15）逐字节不动。
- **dedup 不变**：fallback 文章仍走 `insert_or_get_by_content_hash`，
  同摘要多源条目照常 `dedup_skipped`。
- **质量标记不变**：摘要成文一律 `ContentQuality::Fallback` +
  `extractor_strategy=SummaryFallback`，下游（AI prompt / 报告渲染）
  已有处理路径，无感知。

## 4. 行为变化

| 场景 | 旧 | 新 |
|---|---|---|
| 403 + feed 有摘要 | `failed(http_4xx)` | `fallback_persisted` |
| 403 + feed 无摘要 | `failed(http_4xx)` | 不变 |
| too_large + 有摘要 | `failed(too_large)` | `fallback_persisted` |
| 5xx / 超时（任意摘要状态） | 重试预算路径 | 不变 |
| 预算耗尽折叠（W15） | `failed` | 不变（见 §6 非目标） |

存量 failed 条目**不回收**（用户决策 2026-06-12）：终态行保留审计，
修复只对未来条目生效。

## 5. 测试

`crates/runtime/tests/extract_tests.rs` 新增三例：

1. `extract_fetch_403_with_summary_persists_fallback` — 403 + 摘要 →
   `fallback_persisted`、article 落库、quality=fallback、无 artifact；
2. `extract_fetch_403_without_summary_fails_permanent` — 403 无摘要 →
   `failed(http_4xx)`（锁旧行为不回归）；
3. `extract_fetch_5xx_with_summary_stays_retryable` — 5xx + 摘要 →
   `pending_fetch` 重试路径，**不**提前降级。

## 6. 非目标

- **重试预算耗尽时的兜底**：W15 的耗尽折叠发生在 release SQL 内，最后
  一跳转 `failed` 前不试摘要。让耗尽路径也兜底需要 runtime 预读
  attempt_count 改变 release 时序，复杂度/收益比差（耗尽多为长期瘫痪源，
  摘要价值存疑）。留作观察，不在 W18 范围。
- **github-release-* 的 html_parse 失败**：经查 `summary_fallback` 无字数
  门槛，这些失败是 tag-only release 正文本身为空（feed 与页面都无内容），
  属正确终态，不修。
- 存量 3312 条回收（用户已拍板不做）。

## 7. 关联

- 生产诊断数据与按源处置清单：本次会话 2026-06-12（用户侧换 RSSHub
  全文路由并行进行）。
- [./02-extract.md](./02-extract.md) §策略链 / 兜底——落地后同步。
- [./15-retry-exhaustion-and-reclaim.md](./15-retry-exhaustion-and-reclaim.md)
  ——重试预算语义的权威定义，本设计不触碰。
