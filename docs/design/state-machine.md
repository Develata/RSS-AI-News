# 状态机设计

## 1. 定位

本文档是 Rust 版流程协调层的状态契约。它对应 [工程蓝图 §5](../plan/full-rust-rss-ai-news-blueprint.md) 的状态机草图，但将其细化到每条 transition 的：

- 触发者（谁 UPDATE 这一行）
- 前置条件
- 原子性保证
- 并发冲突解决
- 失败分支
- 后续 transition 的挂钩

状态机是本项目的第一真相源。任一状态列的变更都必须走本文档规定的 transition 之一；不得"绕过状态机直接改字段"。

与之配套的表结构与 SQL 模板见 [storage-schema](./storage-schema.md)。

## 2. 总原则

### 2.1 状态即真相

- `feed_entries.state` / `articles.state` / `article_ai_results.state` / `publish_records.state` 是各自本体对象阶段的唯一表达
- 不存在"内存中的临时状态与数据库状态不一致"的合法时段（除非在同一事务内）
- 进程重启后，系统能力必须仅凭数据库状态恢复

### 2.2 transition 原子性

- 每次 transition 是一次 `UPDATE ... WHERE id = :id AND lease_owner = :owner AND state = :expected`
- 任一条件不满足视为冲突：回滚本次工作，不重试，由 runtime 决定后续
- "业务副作用 + 状态变更"必须合并进一个事务；或副作用对外可见后，再推进状态

### 2.3 lease 模型

所有带 `lease_owner` / `lease_expires_at` 的表遵循同一模型：

- `lease_owner` 非空代表某 worker 已认领
- `lease_expires_at` 是认领过期的绝对时间
- 认领方必须在 `lease_expires_at / 2` 之前完成或主动 renew
- `attempt_count` 在每次 claim 时 +1，不在 reclaim 时递增

**claim 与 reclaim 的状态语义边界**（区分二者，避免相互覆盖）：

- **claim**：从可领取状态原子转入运行态（`pending_fetch → fetching`、`pending → running`、…），同事务写入 `lease_owner / lease_expires_at`、`attempt_count += 1`。claim 不推进任何业务成功终态，仅完成"领取"动作
- **reclaim**：仅作用于运行中状态的过期 lease，把行回滚到下一轮 claim 可见的状态，并清空 lease 字段；`attempt_count` 不递增

reclaim 的具体状态行为按表分别规定：

| 表 | 运行态 | reclaim 后的目标状态 | 备注 |
|---|---|---|---|
| `feed_entries` | `fetching` | `pending_fetch` | 由 ingest 阶段 reclaim 释放 |
| `feed_entries` | `extracting` | `pending_fetch` | 抓取已成功但提取阶段崩溃；下一轮重新走 fetch 路径以保证 body 与 artifact 一致 |
| `article_ai_results` | `running` | `pending` | 见 §4.2 |
| `publish_records` | `snapshot_frozen` / `rendered` / `stored_local` | 维持原状态 | publish 阶段 reclaim 仅清 lease；render / local_store / remote_push 自身幂等可在原状态重入（见 §5.3） |

注：上表是"reclaim 总则"。状态机各章 transition 表中标注"运行态 → 自身保持，lease 清空"的行表示对应表 reclaim 不回滚业务状态；标注"运行态 → 可领取状态"的行表示需要回滚。所有 reclaim SQL 模板见 [storage-schema §5.5](./storage-schema.md#55-lease-reclaim-扫描)。

### 2.4 retry budget

每种状态机定义自己的 `max_attempts`，超出则转入对应的永久失败终态。默认：

- `feed_entries`：`max_attempts = 5`
- `article_ai_results`：`max_attempts = 3`
- `publish_records`：`max_attempts = 5`

具体数值由 `config::app::retry`（即 `[retry]` 段，字段为 `feed_entry_max_attempts` / `ai_max_attempts` / `publish_max_attempts`，见 [config-schema §4](./config-schema.md)）覆盖，不是硬编码。

### 2.5 错误分类

每条失败路径携带两项独立信息：

1. **`last_error_kind`**（存 DB 列）：具体错误变体的 snake_case 名，取自 [error-and-observability §2.3](./error-and-observability.md) 每个错误表的 `error_kind` 列（例如 `http_timeout` / `output_parse` / `github_auth`）。值空间**完全由错误枚举决定**，不含抽象分类
2. **retry class**（仅内存判定，**不入库**）：由错误类型本身的 `ClassifiedError::is_retryable()` 方法返回，取值 `Retryable` / `Permanent` / `Fatal`

**重要**：retry class 绝不从 `last_error_kind` 字符串反推。每次失败时 `runtime` 直接调用错误类型的 `is_retryable()`；`last_error_kind` 只做可观测性用途（查询、聚合、报警）。

处理规则（对每条错误类型在编译期固定）：

- `is_retryable() = true` → 释放 lease 回到前置状态，`attempt_count` 已递增
- `is_retryable() = false`（Permanent）→ 直接转入失败终态
- Fatal 类（如 `StorageError::Corruption`）→ 与 Permanent 同样进失败终态，额外写 `run_events severity='critical'`

典型边界：`PublishError::LocalIoError` 的 `is_retryable()` 在 Rust 实现层按原始 `io::ErrorKind` 判定（`WouldBlock` / `Interrupted` / `TimedOut` → Retryable；其余 Permanent）。文档里的"retryable=false"指默认/典型情况，不替代编译期实现。

**fallback 与 retry class 的优先级**：当某条状态机的 transition 表（如 §3.4）显式列出 fallback 路径（典型为 `ExtractorError::ParseFailed` / `ContentTooShort` 的 summary fallback），分发顺序为：

1. 先按本节通用规则计算 `is_retryable()`，但**不**立即按结果跳转
2. 若该错误在状态机 transition 表中标记有 fallback，则先执行 fallback 分支
3. fallback 成功 → 转入 fallback 成功终态（如 `fallback_persisted`），不进入失败终态
4. fallback 失败或不可用 → 回到第 1 步的 `is_retryable()` 结论：true 走重试，false 走失败终态

这一例外只对状态机文档明确登记 fallback 的错误生效；其他错误仍按 §2.5 通用规则直接分发。

## 3. FeedEntry 状态机

### 3.1 状态集合

| 状态 | 含义 | 终态 |
|---|---|---|
| `discovered` | 刚被 ingest 发现，尚未做去重判断 | 否 |
| `dedup_skipped` | 去重命中，已跳过 | 是（软终态）|
| `pending_fetch` | 去重通过，等待正文抓取 | 否 |
| `fetching` | 某 worker 正在抓详情页 HTML | 否 |
| `extracting` | HTML 已抓到，正在执行正文提取 | 否 |
| `persisted` | 正文已入库，已生成 `articles` 行 | 是（成功终态）|
| `fallback_persisted` | 正文抓取失败，但 summary fallback 成功入库 | 是（软终态）|
| `failed` | 超出 retry budget 或永久错误 | 是（失败终态）|

#### 3.1.1 软终态语义

`dedup_skipped` 与 `fallback_persisted` 是"已经达成阶段目标但与正常成功终态有差异"的状态，与失败终态不同——它们不进入 retry 路径，但允许后续命令在受控条件下再处理：

- **不参与 lease**：原行的 `lease_owner` / `lease_expires_at` 始终保持空，不会被任何 worker 或 reclaim 任务接触
- **状态不可回退**：禁止任何 transition 从软终态回到非终态（包括 `pending_fetch` / `fetching` / `extracting`）；该约束由各阶段 claim SQL 的 `WHERE state IN (...)` 白名单兜底
- **`dedup_skipped` 的再处理**：通过 `backfill --target extract` 仅作用于失败行（`failed`），不重处理软终态行；同一 entry 的重复发现走 §3.2 的 UID/link 去重路径，**不**新建 feed_entries 行
- **`fallback_persisted` 的再处理**：通过 `backfill --target extract` 同样**不**改写原 feed_entries 行，而是在 `articles` 层按 `content_quality='fallback'` 升级到 `high` / `medium` 的策略**新建** AI 任务（见 §4），原 feed_entries 行保持 `fallback_persisted`
- **`reindex` 的影响**：只更新 `articles` / `links` / `categories` 派生表，不读取或更新 `feed_entries.state`，因此与软终态无交互

实现校验：claim SQL 的状态白名单只列非终态；契约测试覆盖"软终态行被错误地传入 claim → UPDATE 影响 0 行"。

### 3.2 transition 表

三层去重在事务层语义不同，注意区分：

- **第一层（UID）**：`UNIQUE(source_id, feed_entry_uid)` 拦在 INSERT 时。**不产生新行**，也不存在 `discovered → dedup_skipped` 这种转移。INSERT 返回约束冲突 / `ON CONFLICT DO NOTHING` 后，runtime 把该次发现聚合到 `run_events entry_dedup_skipped` 事件，不写 `feed_entries` 表。
- **第二层（link_hash）**：在 INSERT 之前由 runtime 做 SELECT 查询。命中已有行时**不插入新行**，走与 UID 层相同的事件日志路径。
- **第三层（content_hash）**：在 `extracting` 之后，**此时已有 `feed_entries` 新行**，所以会产生 `extracting → dedup_skipped` 这个真实的状态转移（新行被标为 dedup_skipped 并关联已有 `article_id`）。

| 起始 | 目标 | 触发者 | 前置条件 | 副作用 | 失败分支 |
|---|---|---|---|---|---|
| (no row) | (no row, event only) | `runtime::ingest::feed_parser` | INSERT 时 `UNIQUE(source_id, feed_entry_uid)` 冲突 | 本轮 ingest 结束时**聚合写一条** `run_events entry_dedup_skipped`（`target_kind='feed_entry'`、`target_id=NULL`、`context_json` 中以数组形式列出 `{existing_id, decision: 'uid_dup'}` 的全部命中项）| — |
| (no row) | (no row, event only) | `runtime::ingest::link_dedup` | INSERT 前 SELECT `link_hash` 命中 | 与上同一条聚合事件，`context_json` 内追加 `{existing_id, decision: 'link_dup'}` 项 | — |
| (insert) | `discovered` | `runtime::ingest::feed_parser` | 通过 UID + link 两层检查 | INSERT 成功 | — |
| `discovered` | `pending_fetch` | 同上 | — | 写 `dedup_decision='fresh'` | — |
| `pending_fetch` | `fetching` | `runtime::ingest::fetch_claim` | claim 成功 | lease 字段填充，`attempt_count += 1` | claim 冲突 → 本轮跳过 |
| `fetching` | `extracting` | `runtime::extract` | HTML 响应抓取成功 | body 暂存 / artifact 写入 | fetch 失败：见 §3.4 |
| `extracting` | `persisted` | 同上 | 提取成功 + 第三层去重通过 + `articles` INSERT 成功 | 写 `articles` 行并回填 `article_id` | 见 §3.4 |
| `extracting` | `fallback_persisted` | 同上 | 提取失败但 summary fallback 可用 | 写 `articles` 行（`content_quality='fallback'`）| — |
| `extracting` | `dedup_skipped` | 同上 | 第三层 `content_hash` 命中已有文章 | 写 `dedup_decision='hash_dup'` + 关联 `article_id` | — |
| `extracting` | `pending_fetch` | 同上 | 提取失败且 retryable，`attempt_count < max` | 清 lease | — |
| `extracting` | `failed` | 同上 | 提取失败且不可重试 或 retry 耗尽 | 写 `last_error*` | — |
| `fetching` / `extracting` | `pending_fetch` | lease reclaim 扫描 | 租约过期 | 清 lease 字段并回滚 `state='pending_fetch'`（见 §2.3 reclaim 总则与 [storage-schema §5.5](./storage-schema.md#55-lease-reclaim-扫描)）；`attempt_count` 已在 claim 时递增，不再变 | 下一轮 claim 重新领取 |

### 3.3 冲突解决

- **双 worker 同时抢同一行 `pending_fetch`**：只有一个 `UPDATE ... RETURNING` 成功，另一个返回 0 行，本次 claim 视为冲突，debug 级记录
- **worker 抢到后进程崩溃**：lease 过期后由 reclaim 释放
- **worker 完成但 COMMIT 前崩溃**：事务回滚，副作用丢失；下一轮 claim 重新领取；`attempt_count` 已在 claim 时递增，不会无限重跑

### 3.4 失败路径

错误变体名以 [error-and-observability §2.3](./error-and-observability.md) 为 canonical source：feed 阶段用 `FeedError`，提取阶段用 `ExtractorError`。

| 失败点 | 错误变体 | is_retryable | 处理 |
|---|---|---|---|
| feed fetch 超时 | `FeedError::HttpTimeout` | true | 回 `pending_fetch`，lease 清空 |
| feed fetch 4xx | `FeedError::HttpStatus { code: 4xx }` | false | 转 `failed`，记 `last_error` |
| feed fetch 5xx | `FeedError::HttpStatus { code: 5xx }` | true | 回 `pending_fetch`；attempt 耗尽后转 `failed` |
| feed fetch 连接失败 | `FeedError::ConnectionFailed` | true | 回 `pending_fetch` |
| feed 过大 | `FeedError::TooLarge` | false | 转 `failed` |
| feed 解析失败 | `FeedError::ParseFailed` | false | 转 `failed` |
| 详情页 fetch 超时 | `ExtractorError::HttpTimeout` | true | 回 `pending_fetch` |
| 详情页 4xx | `ExtractorError::HttpStatus { code: 4xx }` | false | 转 `failed` |
| 详情页 5xx | `ExtractorError::HttpStatus { code: 5xx }` | true | 回 `pending_fetch` |
| 正文过大 | `ExtractorError::TooLarge` | false | 转 `failed` |
| 正文提取失败 | `ExtractorError::ParseFailed` | false | 先尝试 summary fallback，失败则转 `failed` |
| 内容太短 | `ExtractorError::ContentTooShort` | false | 先尝试 summary fallback，失败则转 `failed` |
| 内容哈希冲突（合法重复）| 非错误 | — | 转 `dedup_skipped` |
| fallback 成功 | 非错误 | — | 转 `fallback_persisted` |

### 3.5 观测点

每次 transition 都应：

- 向 `tracing` 发出 span event：`target="feed_entry.transition"`，字段 `id`、`from`、`to`、`reason`
- 失败 transition 按 [error-and-observability §4.3](./error-and-observability.md) 的 canonical 子集决定是否写 `run_events`（只写 `source_fetch_failed` / `entry_permanent_failed`；一般性可重试失败只走 tracing）
- 成功 transition 不写 `run_events`，避免喧闹

### 3.6 验证方式

- 单元测试覆盖每条 transition，包括 claim 冲突路径
- 集成测试至少包含：重入抓取成功、重入抓取失败、fallback 成功、去重 skip、lease 过期后 reclaim 恢复

## 4. Article / AI 状态机

`articles.state` 与 `article_ai_results.state` 是两条相关但独立的状态机。

### 4.1 `articles.state`

| 状态 | 含义 | 终态 |
|---|---|---|
| `persisted` | 正文入库，已准备好接受 AI | 否 |
| `ai_pending` | 已创建至少一行 `article_ai_results` 任务 | 否 |
| `ai_done` | 至少有一行 `article_ai_results.state='succeeded'` | 否 |
| `ready_for_publish` | 符合发布条件（`ai_done` + keep_decision=1 + 最低分数）| 否 |
| `publish_skipped` | AI 判定过滤或不符发布条件 | 是（软终态）|
| `published` | 被至少一条 `publish_items` 引用且 `publish_records` 成功 | 是（成功终态）|
| `retired` | 软删除，首版不启用，作为未来预留 | 是 |

`articles` 本身没有 lease 字段；它的阶段推进**始终**是 `article_ai_results` 或 `publish_records` 状态变化的派生结论，由 `runtime` 在导致派生的同一个事务内 `UPDATE`。不存在独立的 `articles` claim 任务。

#### 4.1.1 `articles.state` transition 表

| 起始 | 目标 | 触发者 | 前置条件 | 副作用 | 失败分支 |
|---|---|---|---|---|---|
| (insert) | `persisted` | `runtime::extract::commit_article` | extract 成功 + 第三层去重通过 + `articles` INSERT 成功 | 写 `articles` 行；同事务回填 `feed_entries.article_id` 与 `feed_entries.state='persisted'` | `UNIQUE(content_hash)` 冲突 → 不插入 articles，`feed_entries.state` 转 `dedup_skipped` (`dedup_decision='hash_dup'`) |
| `persisted` | `ai_pending` | `runtime::ai_run::task_gen` | `INSERT article_ai_results (...) state='pending'` 成功 | 同事务 `UPDATE articles SET state='ai_pending' WHERE id=:id AND state='persisted'` | UPDATE 影响 0 行（被并发改）→ 回滚整事务，下一轮基于真相源重判 |
| `ai_pending` | `ai_done` | `runtime::ai_run::complete` | 当前 `article_ai_results` 转 `succeeded` 且 `keep_decision=1`，但未达发布门槛（`importance_score < min_importance_score`） | 同事务 UPDATE | — |
| `ai_pending` | `ready_for_publish` | 同上 | 同上，但达到发布门槛 | 同事务 UPDATE | — |
| `ai_pending` | `publish_skipped` | 同上 | 当前 `article_ai_results` 转 `filtered`（`keep_decision=0`）且不存在其他 `succeeded` 行 | 同事务 UPDATE | — |
| `ai_done` | `ready_for_publish` | `runtime::ai_run::complete` 或 `backfill` | 新版本 `article_ai_results` 成功且分数达门槛 | 同事务 UPDATE | — |
| `ai_pending` / `ai_done` | 保持 | `runtime::ai_run::complete` | 当前 `article_ai_results` 转 `permanent_failed`，但允许其他 prompt/model 版本继续补跑 | 不更新 `articles.state` | — |
| `ready_for_publish` | `published` | `runtime::publish::local_store` 或 `runtime::publish::remote_push` | 同事务 `publish_records` 进入 `published_local`（本地模式）或 `published_remote`（远端模式）| 批量 `UPDATE articles SET state='published' WHERE id IN (...) AND state='ready_for_publish'` | 任一行 UPDATE 影响 0 行 → 整批 publish 回滚转 `failed`，下一轮基于真相源重新选稿 |
| `persisted` / `ai_pending` / `ai_done` | `retired` | `runtime::admin::retire`（首版不启用） | 管理员命令 | UPDATE | — |
| `persisted` | `ready_for_publish` | `runtime::publish::promote_no_ai` | **AI 关闭模式**：`config.ai.enabled=false` 且 `config.publish.include_unscored=true` | 直接升格；`article_ai_results` 不新建行 | AI 关闭但 `include_unscored=false` → article 停留在 `persisted`，不进入发布路径 |

#### 4.1.2 派生原则

- `articles.state` 的任何变更都必须发生在导致派生的那个事务内
- 没有任何 `runtime` 流程会独立读取 `articles.state` 并推进它，而不同时修改 `article_ai_results` 或 `publish_records`（**例外**：§4.1.3 的 AI 关闭直通路径，此时不存在 `article_ai_results` 可供联动，由 `publish` 阶段在选稿事务中直接升格）
- **`articles` 不承载阶段错误**：`articles.state` 无失败终态；任意阶段失败（fetch / extract / AI / publish）的 `last_error` / `last_error_kind` 写在对应真相源行（`feed_sources` / `feed_entries` / `article_ai_results` / `publish_records`），不写 `articles` 行（`articles` 表也无 `last_error*` 列）。错误传播规则见 [error-and-observability §3.1](./error-and-observability.md#31-能力层--流程协调层)
- `doctor --deep` 会校验 §7.2 列出的跨状态机不变量

#### 4.1.3 AI 关闭 / 无 AI 发布降级

为满足蓝图 §3.1.D 的"AI 可关闭"需求，新增一条非 AI 发布路径：

- 触发条件：`config.ai.enabled = false`，且 `config.publish.include_unscored = true`
- 语义：`persisted → ready_for_publish` 直通，不经过 `ai_pending` / `ai_done`；`article_ai_results` 不新建行
- 发布选稿：`runtime::publish::freeze` 时除选 `ready_for_publish` 外，当 `ai.enabled=false` 时还将 `persisted` 行视为候选并在入选的同事务内升格到 `ready_for_publish`
- 渲染降级：`publish_items` 的 `frozen_summary` 取 `feed_entries.summary_raw`（通过 `articles.origin_feed_entry_id` 关联，参见 [storage-schema §4.6](./storage-schema.md#46-publish_items) 的 `frozen_summary` 列说明 + `crates/storage/src/repo/publish_item.rs` 的 `COALESCE(fe.summary_raw, '') AS summary` 实现）；`frozen_tags_json` 为 `[]`；`frozen_score` 为 NULL
- 若 `ai.enabled=false` 但 `include_unscored=false`，`persisted` 行永不进入发布路径，仅作为历史数据保留（与当前"AI 未跑完前不发布"语义一致）
- **`include_unscored` 不是 AI failure fallback**：当 `ai.enabled=true` 时，即使 `include_unscored=true` 也**不会**触发本节直通路径。具体地：
  - `articles.state` 在 AI 路径中由 `running → permanent_failed` 转换不更新（保持 `ai_pending`），见 §4.2 的"AI 永久失败不更新 articles.state"约定
  - `ai_pending` 状态的 article **不**满足 `freeze` 选稿条件（仅 `ready_for_publish` 进入候选）
  - 因此 AI 永久失败的 article 既不会因为 `include_unscored=true` 自动直出，也不会被同一 publish 跑挑中；只能通过 `backfill --target ai`（新模型 / 修正 prompt）重跑后才能进入 `ai_done → ready_for_publish`
  - 配套配置真值表见 [config-schema §4.1](./config-schema.md#41-aienabled--publishinclude_unscored-真值表)
- `doctor` 在 `ai.enabled=false` 下跳过 `OPENAI_API_KEY` 校验

### 4.2 `article_ai_results.state`

| 状态 | 含义 | 终态 |
|---|---|---|
| `pending` | 已创建任务行，等待 claim；也是 retryable 失败后回落的状态 | 否 |
| `running` | 某 worker 已 claim 正在调用 AI | 否 |
| `succeeded` | AI 调用成功且 JSON 解析成功 | 是（成功终态）|
| `permanent_failed` | 重试耗尽或永久错误 | 是（失败终态）|
| `filtered` | AI 明确判定 `keep_decision=0` | 是（软终态）|

**为什么没有 `retryable_failed`**：retryable 失败不是持久状态，claim SQL 只消费 `state='pending'`。retryable 失败在同一 UPDATE 中把 `state` 直接回写为 `pending`（并更新 `last_error*` / `attempt_count`），下一轮 claim 会重新领取。把它作为独立持久状态会导致 claim 路径绕不开，反而不一致。

### 4.3 `article_ai_results` transition 表

| 起始 | 目标 | 触发者 | 前置条件 | 副作用 | 失败分支 |
|---|---|---|---|---|---|
| (insert) | `pending` | `runtime::ai_run::task_gen` | 存在 `articles.state='persisted'` 且该 `(article_id, prompt_version, output_schema_version, model_id)` 四元组在 `article_ai_results` 表中**无任何现存行**（schema 层 UNIQUE 约束兜底；失败重跑回到 §4.2 `running→pending` 路径，不依赖重新 INSERT）| INSERT；并在同事务 `UPDATE articles.state='ai_pending'` | UNIQUE 冲突 → 说明并发 task_gen 已插入，回滚本次，读取真相源后按现状继续 |
| `pending` | `running` | `runtime::ai_run::claim` | claim 成功 | lease, `attempt += 1`, `started_at` | claim 冲突 → 本轮跳过 |
| `running` | `succeeded` | `runtime::ai_run::complete` | JSON 解析成功 + `keep_decision` 存在 | 写 `summary/tags/score/keep_decision/completed_at`；同步更新 `articles.state` → `ai_done` 或 `ready_for_publish` | — |
| `running` | `filtered` | 同上 | `keep_decision=0` | 更新 `articles.state='publish_skipped'` | — |
| `running` | `pending` | `runtime::ai_run::release_retryable` | 错误分类 retryable 且 `attempt_count < max_attempts` | 清 lease、写 `last_error*`；状态回到 `pending` 等下一轮 claim | — |
| `running` | `permanent_failed` | 同上 | 错误分类 permanent 或 `attempt_count >= max_attempts` | 写 `last_error*`。**不**更新 `articles.state`，让其他 model / 版本仍有机会补跑 | — |

### 4.4 多 model / 多版本并存

一篇 `article` 可有多行 `article_ai_results`，分属不同 `(prompt_version, output_schema_version, model_id)`。

- `articles.state='ai_done'` 的判定："至少一行成功"
- `ready_for_publish` 的判定由 `report::selection` 按选稿策略从多行中选优
- `backfill` 命令会为历史文章生成新版本任务行；不覆盖旧行

### 4.5 失败路径

错误枚举名以 [error-and-observability §2.3](./error-and-observability.md) 为准。retryable 处理路径 = `running→pending`（回落重试），permanent = `running→permanent_failed`。

| 失败点 | 错误变体（error-and-observability §2.3）| is_retryable | 处理 |
|---|---|---|---|
| AI 5xx | `AiError::HttpStatus { code: 5xx }` | true | 回 `pending` |
| AI 超时 | `AiError::HttpTimeout` | true | 回 `pending` |
| AI 连接失败 | `AiError::ConnectionFailed` | true | 回 `pending` |
| AI 429 | `AiError::RateLimited { retry_after }` | true | 回 `pending`；下一轮 claim 通过 `governor` 限速推迟 |
| AI 4xx（非 429）| `AiError::HttpStatus { code: 4xx }` | false | `permanent_failed`，写 `run_events ai_permanent_failed` |
| AI quota 耗尽 | `AiError::QuotaExceeded { message }` | false | `permanent_failed` |
| JSON 解析失败 / 字段缺失 / 字段值非法 | `AiError::InvalidJson` / `MissingField { field }` / `InvalidFieldValue { field, reason }` | false | `permanent_failed` |
| 模型返回空（无 choices） | `AiError::EmptyResponse` | false | `permanent_failed` |
| AI 配置非法 | `AiError::InvalidConfig { .. }` | false | `permanent_failed` |
| `keep_decision=false` | 非错误（AI 自报不入选）| — | `filtered`（写 `run_events ai_content_filtered`） |

### 4.6 观测点

run_events 只写 [error-and-observability §4.3](./error-and-observability.md) 定义的 canonical 子集；状态机的所有 transition 走 `tracing`（不必全部进 run_events）。

- `tracing` 全量：claim / succeeded / pending 回落 / permanent_failed / filtered 都产生 INFO/WARN/ERROR span，用于排错
- `run_events` 子集：`ai_permanent_failed`、`ai_content_filtered` 必写；succeeded / pending 回落不写
- metrics：`rss_ai_call_total{status}`、`rss_ai_call_duration_seconds`

### 4.7 验证方式

- 单元测试覆盖每条 transition
- 契约测试锁定 AI 输出 schema 的向后兼容
- 模拟 429 / 5xx / schema drift 的集成测试

## 5. Publish 状态机

### 5.1 状态集合

| 状态 | 含义 | 终态 |
|---|---|---|
| `pending` | 已创建 `publish_records` 行，等待选稿冻结 | 否 |
| `snapshot_frozen` | 已写入对应 `publish_items`，内容冻结 | 否 |
| `rendered` | Markdown 已渲染（内存或临时）| 否 |
| `stored_local` | 本地落盘成功 | 否（远端模式下的中间态）|
| `published_local` | 本地落盘成功且 `--local-only` 模式不再推远端 | 是（本地模式的成功终态）|
| `published_remote` | 远程推送成功 | 是（远端模式的成功终态）|
| `failed` | 失败终态 | 是 |

### 5.2 transition 表

publish_records 同样使用 §2.3 的 lease 模型。每条非终态业务 transition（`pending → snapshot_frozen`、`snapshot_frozen → rendered`、`rendered → stored_local`、`rendered → published_local`、`stored_local → published_remote`）在执行业务副作用之前必须先通过 [storage-schema §5.7](./storage-schema.md#57-publish_records-的领取) 的 claim SQL 按当前 state 取得 lease；claim 自身不推进 state，仅写 `lease_owner` / `lease_expires_at` 并 `attempt_count += 1`。下表的"触发者"行隐含一次前置 claim，不再单列。

reclaim 行为见 §2.3：publish_records 的 reclaim 仅清 lease 字段、不回滚 state，因为 render / local_store / remote_push 自身幂等，下一轮 claim 可在原中间态继续推进。

| 起始 | 目标 | 触发者 | 前置条件 | 副作用 | 失败分支 |
|---|---|---|---|---|---|
| (insert) | `pending` | `runtime::publish::init` | `idempotency_key` 未冲突 | INSERT；不需要 claim（首次写入即所有权确立）| UNIQUE 冲突 → 视恢复策略 |
| `pending` | `snapshot_frozen` | `runtime::publish::freeze` | 前置 claim 成功 + 选稿返回非空集合 | 批量 INSERT `publish_items`，事务提交，UPDATE state | claim 冲突 → 本轮跳过；空集合 → `failed` (`SnapshotEmpty`) |
| `snapshot_frozen` | `rendered` | `runtime::publish::render` | 前置 claim 成功 + 渲染成功 | UPDATE state，无其他持久副作用 | claim 冲突 → 本轮跳过；渲染错误 → `failed` |
| `rendered` | `stored_local` | `runtime::publish::local_store` | 前置 claim 成功 + 写文件成功 + `publish_records.remote_target` 非空（远端模式）| 写 `local_path`, `local_stored_at`，UPDATE state | IO 失败 → 保持 `rendered`，计入 retry |
| `rendered` | `published_local` | 同上 | 前置 claim 成功 + 写文件成功 + `publish_records.remote_target` 为空（`--local-only` 模式）| 同上；下游同步更新 `articles.state='published'` | IO 失败 → 保持 `rendered`，计入 retry |
| `stored_local` | `published_remote` | `runtime::publish::remote_push` | 前置 claim 成功 + GitHub API 成功 | 写 `commit_sha`, `remote_published_at`，UPDATE state；下游同步更新 `articles.state='published'` | 网络失败 → 保持 `stored_local`，retry；auth 失败 → `failed` |
| 任一非终态 | `failed` | 异常处理 | retry 耗尽或永久错误 | 写 `last_error*` | — |

**模式判定**：`publish_records.remote_target` 由 `runtime::publish::init` 按 CLI flag / 配置写入。`--local-only` 或 `config.publish.github_*` 为空时 `remote_target=NULL`，本轮只走 `published_local`；否则写 `github://owner/repo/branch/path`，走完整远端链路。

### 5.3 幂等与重放

`idempotency_key` 唯一约束是首要防线。

同一 `idempotency_key` 的 `publish_records` 在重放时：

- 当前 state 为成功终态 → skip
- 当前 state 为 `failed` → 生成新 `idempotency_key`（附加版本后缀）作为新批次
- 当前 state 在中间态且 lease 过期 → 从当前 state 继续推进（render / local_store / remote_push 都是幂等可重复的）

`rebuild-report` 命令只读 `publish_items`，不推进状态机，不产生新发布记录。

### 5.4 对下游对象的副作用

`published_remote` 或 `published_local` 中**任一**成功终态达成后，`runtime` 在同一事务中批量将对应 `articles.state` 从 `ready_for_publish` 更新为 `published`。两条终态对下游等价；通过 `publish_records.state` 与 `remote_target` 区分实际发布去向。

若任一 article 的 state 已非 `ready_for_publish`（被并发改动），整批回滚并转 `failed`，下一轮基于新的真相状态重新冻结快照。这是时序一致性的具体兑现。

### 5.5 失败路径

错误变体名以 [error-and-observability §2.3](./error-and-observability.md) 为 canonical source。本阶段使用 `PublishError` 与 `ReportError`。

| 失败点 | 错误变体 | is_retryable | 处理 |
|---|---|---|---|
| 选稿空集 | `PublishError::SnapshotEmpty` | false | `failed` |
| 渲染异常 | `ReportError::*`（变体集由 report crate 内部定义，permanent）| false | `failed` |
| 本地 IO 失败 | `PublishError::LocalIoError` | false | 写 `last_error*`，保持 `rendered` 等下一轮重试（retry 属性在 runtime 层按 `LocalIoError` 的原始 `io::ErrorKind` 判定，WouldBlock / Interrupted 才可重试）|
| GitHub 4xx auth | `PublishError::GitHubAuthFailed` | false | `failed` + critical |
| GitHub 4xx 其它 | `PublishError::GitHubApiError { status: 4xx }` | false | `failed` |
| GitHub 5xx | `PublishError::GitHubApiError { status: 5xx }` | true | 保持 `stored_local` 重试 |
| GitHub 429 | `PublishError::GitHubRateLimit { reset_at }` | true | 保持 `stored_local`；根据 `reset_at` 推迟下一轮 |
| `publish_records` UNIQUE 冲突 | `StorageError::Conflict` | false | 视 §5.3 恢复策略处理 |

### 5.6 观测点

run_events 只写 [error-and-observability §4.3](./error-and-observability.md) 定义的子集：`publish_started`、`publish_succeeded`、`publish_failed`。其它中间 transition（`snapshot_frozen` / `rendered` / `stored_local`）只走 tracing，不进 run_events。

- metrics：`rss_publish_total{category,status}`

### 5.7 验证方式

- 单元测试覆盖每条 transition + 失败回退
- 集成测试：完整 freeze → render → local → remote 链路 + 断电重启恢复
- rebuild-report 契约测试：从 `publish_items` 构造 Markdown 与历史产出 byte-equal

## 6. reindex_job 独立状态轮

### 6.1 定位

reindex 是规则升级触发的批量重算操作（`link_hash` / `content_hash` / `categories`），由 [storage-schema §4.10 `reindex_jobs`](./storage-schema.md#410-reindex_jobs) 表持久化 job 元数据与 checkpoint。

reindex_job **不属于真相源对象状态机**：它不修改任何 `feed_entries.state` / `articles.state` / `article_ai_results.state` / `publish_records.state`。它只更新数据行的 `*_rule_version_id` 外键，并控制 `rule_versions.status` 的两阶段激活时序。因此 §7.2 跨状态机不变量 I1–I8 不受 reindex 直接影响。

### 6.2 状态集合

| 状态 | 语义 | 是否运行态 |
|---|---|---|
| `pending` | reindex_job 已创建、新 `rule_versions` 行 INSERT 为 `status='pending'`、尚未开始扫描或被 reclaim 后等待重领 | 否 |
| `running` | 持有 lease，扫描 + 批量更新进行中；按 `--batch-size` 提交 checkpoint（每批 commit）| 是 |
| `completed` | 全部行已更新；同一终止事务内 `rule_versions.status: pending → active`、旧 active 同 kind 行 → `superseded` + `retired_at` | 否（成功终态）|
| `failed` | 不可恢复失败（如规则签名校验失败、批次重试上限耗尽）；`rule_versions` 仍保持 `pending` | 否（失败终态）|
| `aborted` | 用户主动终止（`reindex --abort`）；管理员决策清理 pending `rule_versions` | 否（取消终态）|

### 6.3 transition 表

| from | to | 触发 | 副作用 | 备注 |
|---|---|---|---|---|
| (无) | `pending` | `cli reindex --target X` 启动 | INSERT `rule_versions` (`status='pending'`) + INSERT `reindex_jobs` (`state='pending'`) 同事务 | 同 target 已有 `pending`/`running` job 时拒绝（partial unique index）|
| `pending` | `running` | runtime claim | 写 `lease_owner` / `lease_expires_at` / `started_at`；`attempt_count += 1` | 见 [storage-schema §5](./storage-schema.md#5-claim--lease-sql-模板) lease 模板 |
| `running` | `running` | 批次完成 | UPDATE `last_processed_id = batch_max_id`；同一批次的数据行 `*_rule_version_id` 已指向 pending 行；批次内 COMMIT | 每批一个 SQLite 事务，避免长事务 |
| `running` | `pending` | lease 过期 reclaim | 清 lease 字段，保留 `last_processed_id`；下次 claim 从 checkpoint 继续 | 与 [§2.3 lease reclaim 总则](#23-lease-模型) 一致；`attempt_count` 不变 |
| `running` | `completed` | `last_processed_id == max(target_table.id)` | 终止事务内：`rule_versions.status: pending → active`，同 kind 旧 active 行 → `superseded` + `retired_at`；`reindex_jobs.state='completed'` + `finished_at` | 关键：版本激活与 job 完成同一事务，对外原子可见 |
| `running` | `failed` | 规则 sha256 不匹配 / 批次重试上限耗尽 / 不可恢复 SQL 错误 | 写 `error` + `finished_at`；**不**激活 `rule_versions`；pending 行保留供管理员介入 | 与 retry budget 不同：reindex 内部批次重试由 runtime 控制；超限后整个 job failed |
| `running` | `aborted` | `cli reindex --abort <job_id>` | 写 `aborted_reason` + `finished_at`；保留 pending `rule_versions` 行直到管理员清理 | 已更新的数据行 `*_rule_version_id` 指向 pending 行；不回滚（数据语义仍正确，因为 active resolver 取的是旧 active）|

### 6.4 active rule resolver

所有读取规则的命令（ingest / extract / ai-run / publish）通过 `active_rule(kind)` resolver 取规则：

```sql
SELECT id, payload_sha256, ... FROM rule_versions
WHERE kind = :kind AND status = 'active';
```

partial unique index `UNIQUE (kind) WHERE status = 'active'` 保证返回 0 或 1 行；首版 migration 后保证返回 1 行。

**reindex 期间的可见性**：

- 旧 active rule 仍 `status='active'`，其它命令使用旧规则不受影响
- 已更新批次的数据行 `*_rule_version_id` 指向 `pending` 行；这些数据行从查询角度（`JOIN rule_versions WHERE status='active'`）查不到当前关联的规则版本——这是设计意图：reindex 期间数据行的"逻辑规则版本"仍由旧 active 决定，pending 行只是"未来某一刻的快照"
- reindex `running → completed` 的瞬间（`pending → active` 同事务），所有引用 pending 行的数据行立刻成为新 active 的成员；旧版降为 `superseded`
- 此设计避免「reindex 期间新写入的数据行该用哪个规则版本」的歧义：新写入数据行始终通过 `active_rule(kind)` 取旧 active 写入；reindex 不感知 reindex 期间的新增行（不变量见 §6.5）

### 6.5 与 ingest 的并发不变量

reindex 期间 ingest 持续运行；由于 reindex 不阻塞 ingest，可能出现「reindex 完成时仍有数据行用旧 active 写入但 last_processed_id 已超过该行 id」的情况。处理约定：

- reindex 完成后立即跑一遍 `doctor --deep`，会发现有 `*_rule_version_id` 仍指向已变 `superseded` 的旧版（这是预期，**不**算违反不变量；记 INFO 不告警）
- 真正需要新规则覆盖这些"漏网行"时，调用方应在 reindex 完成后再启动一次 reindex（同 target），或在 ingest 流量低谷期触发；不引入「reindex 排空 ingest 队列」的机制

### 6.6 失败路径

| 失败类型 | 终态 | 处理 |
|---|---|---|
| 规则 sha256 与配置不匹配 | `failed` | runtime 启动前校验，发现不匹配 → `failed`，不进入 running |
| 批次 SQL 错误（瞬时） | 重试 → `running` 内部重试 | 重试上限由 `[retry] reindex_max_attempts`（待 W3 加入）决定；超限 → `failed` |
| 批次 SQL 错误（永久，如 schema 不匹配） | `failed` | 终止；管理员介入修 schema 或回退 reindex 启动决策 |
| 进程崩溃 / lease 过期 | reclaim → `pending` | 下次 claim 从 `last_processed_id` 继续 |
| 用户 abort | `aborted` | 见 §6.3 备注；不自动清理 pending rule_versions |

### 6.7 观测点

- 每次 `state` 变更写 `run_events`（stage=`reindex`）
- batch 提交不写 `run_events`（避免膨胀）；只在 trace span 中记录
- `running → completed` 写 INFO 事件，包含：target / rule_version_id / 受影响行数 / 耗时
- `running → failed` / `aborted` 写 error / warn 事件

### 6.8 验证方式

- 单元测试：每条 transition + 失败回退（lease 过期、sha256 不匹配、abort、批次 SQL 错误）
- 集成测试：完整 `pending → running → completed` 链路 + crash-after-batch 恢复 + ingest 期间 reindex 不污染 active resolver
- 隔离测试：reindex 期间调用 ingest，验证 ingest 取到旧 active rule（payload_sha256 比对）

## 7. 并发与时序一致性

### 7.1 冲突分类

| 冲突 | 检测方式 | 解决 |
|---|---|---|
| 双 worker 抢同一行 | `UPDATE ... RETURNING` 返回 0 行 | 放弃本次，不记 error |
| lease 过期被 reclaim 后原 worker 完成 | `WHERE lease_owner = :owner` 返回 0 行 | 放弃写入，副作用作废 |
| 状态被并发改动 | `WHERE state = :expected` 返回 0 行 | 放弃，重新读取真相源 |
| UNIQUE 冲突 | SQL error | 视业务语义决定幂等 skip 或错误 |

### 7.2 跨状态机不变量

这份列表是 `doctor --deep` 扫描的 **canonical 不变量集**。CLI 中列出的任何 `--deep` 示例必须是本集合的子集。

- **I1**：`feed_entries.state='persisted'` ⇒ `feed_entries.article_id` 非空，且对应 `articles` 行存在
- **I2**：`articles.state='ai_pending'` ⇒ 至少存在一行 `article_ai_results` 关联到该 article（无论 state）
- **I3**：`articles.state='ai_done'` ⇒ 至少一行 `article_ai_results.state='succeeded'`
- **I4**：`articles.state='ready_for_publish'` ⇒ XOR 两路径之一（在 SQL 层由 EXISTS/NOT EXISTS 天然互斥，可执行查询见 [cli-semantics §4.4](./cli-semantics.md) canonical SQL）：  
  **I4.a（AI 路径）**：`EXISTS` `article_ai_results` 满足 `article_id = articles.id AND state='succeeded' AND keep_decision=1`  
  **I4.b（AI 关闭直通路径）**：`NOT EXISTS` 任何 `article_ai_results` 行（`WHERE article_id = articles.id`）。注意：直通路径的判定锚点是"AI 任务从未被创建"（与 §4.1.3 "AI 关闭模式 article_ai_results 不新建行" 一致），而非"无可保留 AI 结果"。`filtered` / `permanent_failed` / `pending` / `running` 都是 AI 路径的中间或终态，存在即排除直通可能性。  
  互斥说明：I4.a 与 I4.b 不是 `EXISTS succeeded+keep` 的二值否定，而是"有任何 AI 行"与"无任何 AI 行"的二分。当 article 处于 AI 路径但停留在 `permanent_failed` / `filtered` 时，I4.a 与 I4.b 都不成立 ⇒ article 必然不在 `ready_for_publish` 状态（state-machine §4.2 保证），I4 自动满足空集前提。  
- **I4'**（freeze 后 publish_items 绑定一致性）：对任意 `publish_items` 行（按行枚举，与 I4 解耦）：  
  - `article_ai_result_id IS NOT NULL` ⇒ 所引用的 `article_ai_results.state='succeeded' AND keep_decision=1`（确保 freeze 时绑定了真实可保留的 AI 证据）  
  - `article_ai_result_id IS NULL` ⇒ 同 article 上 `NOT EXISTS` 任何 `article_ai_results` 行（确保 freeze 时该 article 走的是真正的 AI 关闭直通路径，未在 AI 失败/过滤后被误降级）  
  schema CHECK（`article_ai_result_id` 与 `frozen_score` 同 NULL）保证每行只能落入一支；freeze 事务保证同一 article 在同一 publish snapshot 内只产出一条 `publish_items`，路径绑定一旦写入不可篡改。canonical SQL 见 [cli-semantics §4.4](./cli-semantics.md)。
- **I5**：`articles.state='published'` ⇒ 至少一行 `publish_items` 引用，且对应 `publish_records.state` 为成功终态（`published_remote` 或 `published_local`）
- **I6**：`publish_records.state IN ('published_remote', 'published_local')` ⇒ 被其引用的所有 `articles.state` 已同步为 `published`
- **I7**：`publish_items` 永远指向存在的 `articles`；若 `article_ai_result_id IS NOT NULL`，则指向存在的 `article_ai_results`（外键 + NULL 判定共同保证）；`article_ai_result_id` 与 `frozen_score` 必须同时 NULL 或同时非空（CHECK）
- **I8**：不存在 `article_ai_results.state='running'` 且 `lease_expires_at < NOW()` 的行（若存在说明 reclaim 任务落后）

上述不变量由 `runtime` 在事务内保证；通过 `doctor --deep` 扫描持续验证。若发现破坏，写 critical 事件并人工介入。

## 8. 与宪法的对齐检查

- §5.5 幂等与并发：claim + lease + `WHERE lease_owner = :owner AND state = :expected` ✓
- §5.1 失败优先：每条 transition 都有失败分支 ✓
- §5.2 可观测性内建：每条 transition 发 tracing event，关键点写 `run_events` ✓
- §5.3 验证先行：每条 transition 规定了验证方式 ✓
- §5.4 版本责任：状态 enum 变更必须 migration + 数据迁移 ✓
- §6.2 退出路径：三条状态机都有明确失败终态与成功终态 ✓
