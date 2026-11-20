# 02 — 正文提取段

本章详解主链路第二段：从 `feed_entries.state=pending_fetch` 到 `articles` 入库。

```text
pending_fetch (claim+lease)
  → fetching   (HTML 抓取)
    → 抓取永久失败（4xx/too_large/invalid_url）→ summary fallback
      → 摘要可用 → fallback_persisted（W18，见 ./17-extract-fetch-fallback.md）
      → 无摘要   → failed
    → 抓取可重试失败（超时/5xx）→ 回 pending_fetch + next_attempt_at 冷却（W15 预算路径，§5.3）
  → extracting (策略链提取)
    → 第三层 content_hash 去重
    → 成功 → INSERT articles + feed_entries.state='persisted'
    → 失败 → summary fallback → fallback_persisted
    → fallback 也失败 → failed
```

## 1. 边界

本章覆盖：
- 详情页 HTML 抓取（含限流、媒体类型过滤、大小限制）
- 策略链提取（readability / summary-fallback）
- 第三层 `content_hash` 去重
- `articles` 表 INSERT + `feed_entries` 状态推进
- `replay --kind html` 子命令的契约

**不覆盖**：
- feed 列表抓取 → [./01-feed.md](./01-feed.md)
- AI 任务生成 → [./03-ai.md](./03-ai.md)

## 2. HTML 抓取

`HtmlFetcher` trait 在 [`crates/extractor/src/fetcher.rs`](../../crates/extractor/src/fetcher.rs) 定义。
生产实现是 reqwest-based fetcher，与 feed crate 的 client 独立。

### 2.1 抓取约束

每次抓取直接 GET，自动跟随最多5次 redirect。超时来自 `http.timeout_seconds`，
payload 上限来自 `extractor.max_body_bytes`；先检查 Content-Length，再逐 chunk 检查实际长度。
不单独发 HEAD，也不强制 Content-Type 白名单；非 HTML 内容由策略解析失败路径处理。
User-Agent 由 fetcher 构造器设置。

## 3. 策略链

`ContentStrategy` trait 在 [`crates/extractor/src/strategy.rs`](../../crates/extractor/src/strategy.rs) 定义。
当前策略链有 2 个，按顺序尝试：

| 顺序 | 策略 | 实现 |
|---|---|---|
| 1 | Readability | `readability-rust` crate 适配，基于 Mozilla Readability 算法 |
| 2 | SummaryFallback | 使用 `feed_entries.summary_raw` 作为正文，标记 `content_quality='fallback'` |

每个策略的契约：
- 输入：原始 HTML payload + `ArticleFetchTask`
- 输出：`Result<ExtractedArticle, ExtractorError>`
- `ExtractedArticle` 包含：正文 text、HTML、quality 标记

成功条件：策略返回 `Ok` 且内容长度 ≥ `extractor.min_body_chars`。
Readability 本身也会拒绝空正文；不满足配置最小字数时继续降级。

### 3.1 content_quality 分级

| Quality | 来源 | 处理 |
|---|---|---|
| `High` | Readability 命中且字数充足 | 正常入库 |
| `Medium` | Readability 命中但字数 borderline | 正常入库 |
| `Fallback` | summary fallback 兜底 | 入库为 `fallback_persisted` |

quality 由 strategy 自己决定。AI 阶段会跳过 `Fallback` 行（避免分析 summary），见 [./03-ai.md](./03-ai.md)。

## 4. 第三层去重：content_hash

正文提取使用 SHA-256 计算 content_hash，storage::insert_or_get_by_content_hash
依靠数据库唯一约束原子插入或返回已有 article，禁止 SELECT-exists 再 INSERT。
命中后 feed_entry 进入 dedup_skipped 并关联已有 article；schema/claim 状态由 storage 负责。

命中时已有 article 的字段保持不变（先写者为准，`ON CONFLICT DO NOTHING`）：这次提取新写入的 html artifact
**不会**补到已有 article 的 `body_html_artifact_id`。已有 article 可能本来就没有 artifact（`on_failure` 下提取成功、
`off`、采样未命中、写入失败，或被 TTL 清理后置 NULL），即便如此也不补挂。新 artifact 以 feed entry id 为 key，
写入成功且未被清理时，`replay --kind html --key <feed_entry_id>` 仍可回放，`--diff` 经 `feed_entries.article_id`
找到对应 article（R3 审查遗留，2026-11-16 定为既定语义）。

### 4.1 与一/二层的区别

- 一/二层在 INSERT 前发生，**不**产生新 `feed_entries` 行
- 三层发生在 `extracting` 阶段，**已有 feed_entries 新行**，所以产生真实的 transition

## 5. Extract Flow

由 `ExtractFlow` 在 [`crates/runtime/src/flows/extract.rs`](../../crates/runtime/src/flows/extract.rs) 编排：

```text
run_started_at = now()                      -- 本 run 的 cooldown 截止点
claimed_ids = []                            -- 本 run 已领取过的 id
loop:
  1. claim_pending_fetch(batch_size, run_started_at, claimed_ids) → 最多 N 行 feed_entries
     UPDATE feed_entries SET state='fetching', lease_owner=?, attempt_count += 1
     WHERE state='pending_fetch' AND (lease_expires_at IS NULL OR lease_expires_at < now)
       AND (next_attempt_at IS NULL OR next_attempt_at <= run_started_at)
       AND id NOT IN claimed_ids
     ORDER BY discovered_at ASC LIMIT N RETURNING *
  2. 对每行：
     a. HTTP GET → raw HTML（超时/大小边界）
     b. 永久抓取失败才懒读摘要并尝试 fallback
     c. `always` / `sampled`：写入 raw_artifact
     d. 执行配置的提取策略；`on_failure` 且无策略成功时才写 raw_artifact
     e. 策略链尝试 → ExtractedArticle 或 fallback
     f. 计算 content_hash，原子 insert-or-get
     g. 命中 → feed_entries='dedup_skipped'，关联 article_id
     h. 未命中 → INSERT articles，feed_entries='persisted'
     i. fallback 命中 → INSERT articles (quality='fallback')，feed_entries='fallback_persisted'
  3. claim 为空 → break；达到 max_batches → break；否则继续
     （retryable 失败**不**终止循环，见 §5.3）
```

### 5.1 batch_size 与 max_batches

- CLI `--batch-size`（ingest 默认50）：单批 claim 行数
- `app.runtime.max_batches_per_run`（默认 10）：单次 run 最多跑几批；`0` 表示不限
- 触达 max_batches → INFO 日志 + exit 0（**不**视为失败）

### 5.2 并发与 lease

滚动 JoinSet 将存活任务数限制在 `http.concurrent_fetches.max(1)`，不为整个 claim 批次预建等待任务。
claim 行集合仍占 O(batch size) 内存；CLI 批大小范围见 [09-cli-and-runtime](./09-cli-and-runtime.md)。

claim SQL 用 `FOR UPDATE SKIP LOCKED`（PG）或单条 UPDATE...RETURNING 的数据库写锁（SQLite）保证并发安全。
lease 字段约束见 [./08-state-machines.md](./08-state-machines.md) §2.3 + [./05-storage.md](./05-storage.md)。

### 5.3 retryable 失败冷却（2026-11-20）

- retryable 失败（超时 / 5xx / 连接失败，且预算未尽）回 `pending_fetch`，并在同一条
  owner-guarded release UPDATE 内写 `next_attempt_at = max(release 时刻, run_started_at) + backoff`
  （时刻在 release 前采样，策略链耗时不缩短冷却）。
- backoff 按刚失败的 attempt 有界指数增长：10 / 20 / 40 / 80 min，上限 2 h
  （`RETRY_BACKOFF_BASE` / `RETRY_BACKOFF_CAP`，`flows/extract.rs`）。
- claim 用 run 开始时刻作 cutoff，且 deadline 总晚于 run 开始，所以本 run 释放的行在本 run 内
  不会再被领取（即使其冷却在 run 结束前到期）；冷却行在 `LIMIT` 前被过滤，不占批次名额。
  上一 run 写下、在本 run 开始后才到期的 deadline 同样留到下一 run。
- claim 另外排除本 run 已领取过的 id：每行每 run 至多被领取一次。这覆盖 release 落空的情形
  （lease 过期被并发 run reclaim，owner guard 拦下 release，行回到 `pending_fetch` 且无冷却）
  以及并发 backfill reset。跨 run 的过期 lease 恢复不变。
- SQLite 以小数位数可变的 RFC3339 TEXT 存时间，同一秒内字典序 ≠ 时刻序，冷却比较用
  `julianday()`（毫秒精度）；PG 直接比较 `TIMESTAMPTZ`。
- 循环继续领取其余可领取行，直到 claim 为空或达到 `max_batches`。`retryable_deferred`
  表示本 run 有行进入冷却，可与 `max_batches_reached` 同时为 true。
- 预算、最后一次尝试折叠为 `failed`、`terminalize_exhausted` sweep、过期 lease reclaim、
  永久失败与 summary fallback 均不变；backfill reset 清空 `next_attempt_at`。
- claim 仍按 `discovered_at ASC`：可领取的旧积压大于 `batch_size × max_batches` 时，新条目
  仍可能整轮排不上（`oldest_first_backlog_still_starves_fresh_entries_under_cap` 记录此限制）。
  冷却只保证一篇失败文章不再阻断整轮，不保证新文章能进入发布窗口。

**提案（未实现）：有界 fresh-first claim。** 把可领取行分成两类：fresh =
`COALESCE(published_at, discovered_at) >= now - 发布窗口`（与发布阶段的 48 h 窗口同口径；
`published_at` 可为空故回退 `discovered_at`），其余为 backlog。每批 N 行中至少保留
`R = max(1, ⌈N/4⌉)` 个名额给 backlog（按 `discovered_at ASC`），其余 `N − R` 给 fresh
（同样 `ASC`，先救最接近出窗口的）；任一类不足时把名额让给另一类。`batch_size = 1` 时按
claim 轮次分配（每 4 轮至少 1 轮取 backlog）。总批次上限、冷却、预算与 `claimed_ids` 排除
均照旧。它只给新条目进入抽取的机会，发布仍取决于后续 AI / publish 阶段。

## 6. RawArtifact 留档（HTML）

依据 `config.artifact.retention_policy` 写入 `raw_artifacts` 表：

- 写入时机：`always` / `sampled` 在策略链之前（HTML 已抓到，未解析），独立 commit，策略 panic 时
  artifact 仍在；`on_failure` 在策略链结束后、仅当无策略提取成功（retryable、fallback、永久失败）时写入
- `kind = 'html_payload'`
- `artifact_key = feed_entries.id`（十进制字符串）

详见 [./10-replay-and-backfill.md](./10-replay-and-backfill.md)。

## 7. replay --kind html

从 raw_artifact 重新跑策略链，对比与现有 article 的差异：

```bash
rss-ai-news replay --kind html --key <feed_entry_id> --diff
```

流程：
1. SELECT raw_artifacts WHERE kind='html_payload' AND artifact_key=?
2. 调用 `ReadabilityStrategy.extract(payload, meta)`
3. 与 `articles` 中现有正文 diff（按 normalized whitespace）
4. 输出差异 + 状态报告

**不**改写数据库（read-only 模式）。详见 [./10-replay-and-backfill.md](./10-replay-and-backfill.md)。

## 8. 失败路径速查

| 失败点 | 错误变体 | retryable | 处理 |
|---|---|---|---|
| HTML fetch 超时 | `ExtractorError::HttpTimeout` | true | 回 `pending_fetch` + 冷却（§5.3） |
| HTML 4xx | `ExtractorError::HttpStatus { 4xx }` | false | 尝试 fallback（W18）；无摘要则 `failed` |
| HTML 5xx | `ExtractorError::HttpStatus { 5xx }` | true | 回 `pending_fetch` + 冷却（§5.3） |
| payload 过大 | `ExtractorError::TooLarge` | false | 尝试 fallback（W18）；无摘要则 `failed` |
| 提取失败 | `ExtractorError::ParseFailed` | false | 尝试 fallback；失败则 `failed` |
| 内容太短 | `ExtractorError::ContentTooShort` | false | 尝试 fallback；失败则 `failed` |
| content_hash 命中 | 非错误 | — | 转 `dedup_skipped` |
| fallback 成功 | 非错误 | — | 转 `fallback_persisted` |

## 9. 配置关键项

参考 [./06-config.md](./06-config.md)：

```toml
[http]
timeout_seconds = 30
concurrent_fetches = 5

[extractor]
strategy_order = ["readability", "summary_fallback"]
max_body_bytes = 5242880
min_body_chars = 200
```

## 10. 当前实现入口

| 内容 | 路径 |
|---|---|
| Extract Flow | [`crates/runtime/src/flows/extract.rs`](../../crates/runtime/src/flows/extract.rs) |
| HtmlFetcher | [`crates/extractor/src/fetcher.rs`](../../crates/extractor/src/fetcher.rs) |
| ContentStrategy | [`crates/extractor/src/strategy.rs`](../../crates/extractor/src/strategy.rs) |
| ExtractorError | [`crates/extractor/src/error.rs`](../../crates/extractor/src/error.rs) |
| ArticleRepository | [`crates/storage/src/repo/article.rs`](../../crates/storage/src/repo/article.rs) |
| replay CLI | [`crates/cli/src/commands/replay.rs`](../../crates/cli/src/commands/replay.rs) |
| 集成测试 | [`crates/runtime/tests/extract_tests.rs`](../../crates/runtime/tests/extract_tests.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。

## 运行装配与资源约束（2026-09-10）

CLI 按 strategy_order 装配 Readability；unknown/重复/空列表在结构校验时拒绝。
summary_fallback 仍是最终降级步骤（兼容已有行为），不是网络成功前执行的策略。
Readability 成文须满足 min_body_chars；短摘要 fallback 保留既有允许策略。
claim 后正常成功路径不读 summary_raw；只有永久抓取失败或解析链耗尽才按 id 懒读取。
超时/5xx 保持跨 run 重试（冷却见 §5.3）。HTTP 期间不持有数据库事务。
Feed/HTML 用有上限的分块 Vec 累计后直接移交，保留 Content-Length 与实际读取双重检查。
