# 08 — 状态机集中说明

本章集中说明系统的 4 个核心状态机及其跨状态机不变量。这是"阶段驱动"模式的骨架体现。
每段管线在自己的章节里只描述能力实现，状态契约的真相源在本章。

## 1. 状态机总览

| 状态机 | Rust 类型 | 持久化表 | 关联管线段 |
|---|---|---|---|
| FeedEntry | `FeedEntryState` | `feed_entries.state` | [./01-feed.md](./01-feed.md) / [./02-extract.md](./02-extract.md) |
| Article | `ArticleState` | `articles.state` | [./02-extract.md](./02-extract.md) / [./03-ai.md](./03-ai.md) / [./04-publish.md](./04-publish.md) |
| AiResult | `AiResultState` | `article_ai_results.state` | [./03-ai.md](./03-ai.md) |
| Publish | `PublishState` | `publish_records.state` | [./04-publish.md](./04-publish.md) |

四者协同推进，但**没有任何一个状态机直接读改另一个状态机的状态字段**。跨状态机
推进只在事务边界发生（见 §6）。

辅助状态轮（不在 4 状态机内，但同等重要）：

- `ReindexJobState`：规则版本升级任务轮，见 [../adr/0004-active-rule-resolver-partial-unique.md](../adr/0004-active-rule-resolver-partial-unique.md)
- `RuleVersionStatus`：`rule_versions` 的 pending / active / superseded 三态

## 2. 通用约定

### 2.1 状态即真相

任何 worker 重启后必须只读数据库状态字段就能恢复行为。**禁止**用进程内缓存推断状态。
这是宪法 §3.4 单一真相源的直接推论。

### 2.2 transition 原子性

每次 transition 必须在**同一个事务**内完成"读 state → 校验前置 → 写 state + 副作用"。
不允许"读完 state、做点事、再写 state"这种拉长事务的写法。

实际落地用 `UPDATE ... WHERE state IN (允许的前置)` + 比较 affected rows，影响 0 行 = 并发被
抢走，本次操作放弃。

### 2.3 lease 模型

需要租约的状态：`fetching` / `extracting`（FeedEntry）、`running`（AiResult）、`running`（ReindexJob）、
`pending → snapshot_frozen` 中间态（Publish）。租约持有方信息：

- `lease_owner` — worker 标识
- `lease_expires_at` — 绝对过期时间
- `attempt_count` — 已尝试次数

reclaim 任务扫到过期租约，把状态回退到 `pending` 类前置态，清空 lease 字段，**不**重置
attempt_count（避免无限重试）。

### 2.4 retry budget

每个状态机的 retryable 失败次数有上限：

| 状态机 | 配置项 | 默认值 |
|---|---|---|
| FeedEntry | `retry.feed_max_attempts` | 5 |
| AiResult | `retry.ai_max_attempts` | 3 |
| Publish | `retry.publish_max_attempts` | 3 |
| ReindexJob | `retry.reindex_max_attempts` | 3 |

超限后 retryable 失败也会转为终态 `failed`。详细配置见 [./06-config.md](./06-config.md)。

### 2.5 错误分类

错误模型见 [./11-error-and-recovery.md](./11-error-and-recovery.md)。本章只关注 transition
触发，不重复定义错误类型。

## 3. FeedEntry 状态机

承载"发现 → 去重 → 抓取 → 提取 → 入库"。Rust 定义：
[`crates/domain/src/state.rs`](../../crates/domain/src/state.rs) `FeedEntryState`。

### 3.1 状态集合

| 状态 | 持久化值 | 终态 | 含义 |
|---|---|---|---|
| Discovered | `discovered` | 否 | 刚被 ingest 发现 |
| DedupSkipped | `dedup_skipped` | 软终态 | 三层去重命中 |
| PendingFetch | `pending_fetch` | 否 | 等待详情页抓取 |
| Fetching | `fetching` | 否 | worker 在抓 HTML |
| Extracting | `extracting` | 否 | HTML 已抓，正在提取正文 |
| Persisted | `persisted` | 成功终态 | 正文入库，Article 已生成 |
| FallbackPersisted | `fallback_persisted` | 软终态 | 提取失败但 summary fallback 入库 |
| Failed | `failed` | 失败终态 | 超 retry budget 或永久错误 |

### 3.2 软终态语义

`DedupSkipped` 与 `FallbackPersisted` 是"已达成阶段目标但不属于成功正路"的状态：
- **不参与 lease**：claim SQL 的 `WHERE state IN (...)` 白名单不包含软终态
- **状态不可回退**：禁止从软终态转回非终态
- **`backfill --target extract`** 只作用于 `Failed`，软终态行**不**改写

### 3.3 关键 transition

完整 transition 表在旧 design 文档中有详细列出（[../../docs-backup/design/state-machine.md](../../docs-backup/design/state-machine.md) §3.2）。
本章只列出**非显然**的 transition：

- **去重三层语义不同**：UID 与 link 层是 INSERT 前/时的判定，**不产生新行**；hash 层是
  `extracting → dedup_skipped`，**有真实状态转移**
- **lease 过期 reclaim**：`Fetching` / `Extracting` 经 reclaim 回 `PendingFetch`，attempt_count
  保留
- **失败分支**：retryable 错误回 `PendingFetch`；超 budget 或不可重试转 `Failed`，写
  `last_error*` 列

## 4. Article 状态机

承载文章在 AI / Publish 间的派生状态。Rust 定义：`ArticleState`。

### 4.1 状态集合

| 状态 | 持久化值 | 终态 | 含义 |
|---|---|---|---|
| Persisted | `persisted` | 否 | 正文入库，待 AI |
| AiPending | `ai_pending` | 否 | 已创建 AiResult pending 行 |
| AiDone | `ai_done` | 否 | 至少 1 行 AiResult succeeded，但未达发布门槛 |
| ReadyForPublish | `ready_for_publish` | 否 | 符合发布门槛 |
| PublishSkipped | `publish_skipped` | 软终态 | AI 判定过滤或不符发布条件 |
| Published | `published` | 成功终态 | 被 PublishItem 引用且 PublishRecord 成功 |
| Retired | `retired` | 终态 | 软删除（首版预留，未启用） |

### 4.2 派生规则（critical）

**`articles.state` 永远是派生**：

- `articles` **没有** lease 字段
- 任何 `articles.state` 变更必须发生在 `article_ai_results` 或 `publish_records` 状态变化的**同一事务**内
- **`articles` 不承载阶段错误**：失败信息写在对应真相源行，`articles` 表本身无 `last_error*` 列

例外：**AI 关闭直通模式**（`config.ai.enabled = false` + `config.publish.include_unscored = true`）
时，`publish::freeze` 在选稿事务内直接把 `Persisted → ReadyForPublish` 升格，不经过 AI 路径。
详见 [./03-ai.md](./03-ai.md) §AI-off 直通 与 [./06-config.md](./06-config.md) ai/publish 真值表。

### 4.3 `include_unscored` 不是 AI failure fallback

明确语义边界：

- `ai.enabled = true` 时，`include_unscored=true` **不会**让 AI 失败的 article 自动直出
- AI 永久失败的 article 保持 `AiPending`，不进入 freeze 选稿
- 唯一恢复路径：`backfill --target ai`（新模型 / 修 prompt）重跑成功

## 5. AiResult 状态机

承载单次 AI 调用结果。Rust 定义：`AiResultState`。

### 5.1 状态集合

| 状态 | 持久化值 | 终态 | 含义 |
|---|---|---|---|
| Pending | `pending` | 否 | 已创建任务行，等 claim；retryable 失败回落到此 |
| Running | `running` | 否 | worker 已 claim 在调 AI |
| Succeeded | `succeeded` | 成功终态 | LLM 调用成功 + JSON 解析成功 |
| PermanentFailed | `permanent_failed` | 失败终态 | 重试耗尽或永久错误 |
| Filtered | `filtered` | 软终态 | AI 明确判定 `keep_decision=0` |

### 5.2 多版本并存

一篇 article 可以有多行 `article_ai_results`（不同 `prompt_version` × `output_schema_version` ×
`model_id`）。`articles.state` 由所有 AiResult 行**联合决定**：

- 任一 Succeeded → 至少 `AiDone`，分数达门槛 → `ReadyForPublish`
- 全部 Failed 且无 Succeeded → 保持 `AiPending`（等待新版本补跑）
- 全部 Filtered → `PublishSkipped`

### 5.3 retryable 不作为持久状态

retryable 失败**不**作为独立状态。语义上"暂时失败、稍后再试"由 worker 把 `Running` 直接
转回 `Pending`（保留 attempt_count）。

## 6. Publish 状态机

承载发布批次的"冻结 → 渲染 → 落盘 → 推送"。Rust 定义：`PublishState`。

### 6.1 状态集合

| 状态 | 持久化值 | 终态 | 含义 |
|---|---|---|---|
| Pending | `pending` | 否 | PublishRecord 已建，等待选稿冻结 |
| SnapshotFrozen | `snapshot_frozen` | 否 | PublishItem 已写入，内容冻结 |
| Rendered | `rendered` | 否 | Markdown 渲染完成（无持久副作用，可回放） |
| StoredLocal | `stored_local` | 否 | 已写本地 fs target |
| PublishedLocal | `published_local` | 成功终态（仅本地模式）| 本地模式下不推 GitHub 即结束 |
| PublishedRemote | `published_remote` | 成功终态 | GitHub 推送成功 |
| Failed | `failed` | 失败终态 | 中途任一步永久失败 |

### 6.2 不可变契约

`SnapshotFrozen` 之后，`publish_items.frozen_*` 列**只读不改**。即使源 article 后续被
`backfill ai` 改了 AI 结果，已冻结的 PublishItem 仍然引用旧值。这是
[../adr/0003-publish-snapshot-immutable.md](../adr/0003-publish-snapshot-immutable.md)固化的边界。

### 6.3 重放语义

`Rendered → StoredLocal → PublishedRemote` 是可回放阶段：`rebuild-report` 子命令从
`SnapshotFrozen` 重新渲染并对比 byte-equal。详见 [./04-publish.md](./04-publish.md)。

## 7. 跨状态机不变量

任何 worker 在任何时刻看 4 个状态机的联合视图，以下必须成立：

1. `feed_entries.state = persisted` ⇒ 存在 `articles` 行且 `feed_entries.article_id` 指向它
2. `articles.state = ai_pending` ⇒ 存在 ≥1 行 `article_ai_results` (state ∈ {pending, running, succeeded, filtered, permanent_failed})
3. `articles.state = ai_done` ⇒ 存在 ≥1 行 `article_ai_results.state = succeeded`
4. `articles.state = ready_for_publish` ⇒ §3 不变量 + 分数达门槛 + keep_decision=1
5. `articles.state = published` ⇒ 存在 `publish_items.article_id = articles.id` 且对应
   `publish_records.state ∈ {published_local, published_remote}`
6. `publish_records.state = snapshot_frozen` ⇒ 存在 `publish_items` 行引用本 record

`doctor --deep` 会全表扫描验证这 6 条。详见 [./07-observability.md](./07-observability.md)。

## 8. 当前实现入口

| 内容 | 路径 |
|---|---|
| 4 个状态 enum 定义 | [`crates/domain/src/state.rs`](../../crates/domain/src/state.rs) |
| transition 实现（FeedEntry / Article / AiResult） | [`crates/runtime/src/flows/`](../../crates/runtime/src/flows/) |
| claim + lease SQL | [`crates/storage/src/repo/`](../../crates/storage/src/repo/) |
| state-machine 契约测试 | `#[cfg(test)]` 模块在 [`crates/domain/src/state.rs`](../../crates/domain/src/state.rs) + 集成测试在 [`crates/runtime/tests/`](../../crates/runtime/tests/) |
| doctor 深度扫描（不变量校验） | [`crates/cli/src/commands/doctor.rs`](../../crates/cli/src/commands/doctor.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md)登记漂移。
