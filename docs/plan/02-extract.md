# 02 — 正文提取段

本章详解主链路第二段：从 `feed_entries.state=pending_fetch` 到 `articles` 入库。

```text
pending_fetch (claim+lease)
  → fetching   (HTML 抓取)
    → 抓取永久失败（4xx/too_large/invalid_url）→ summary fallback
      → 摘要可用 → fallback_persisted（W18，见 ./17-extract-fetch-fallback.md）
      → 无摘要   → failed
    → 抓取可重试失败（超时/5xx）→ 回 pending_fetch（W15 预算路径）
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

### 4.1 与一/二层的区别

- 一/二层在 INSERT 前发生，**不**产生新 `feed_entries` 行
- 三层发生在 `extracting` 阶段，**已有 feed_entries 新行**，所以产生真实的 transition

## 5. Extract Flow

由 `ExtractFlow` 在 [`crates/runtime/src/flows/extract.rs`](../../crates/runtime/src/flows/extract.rs) 编排：

```text
loop:
  1. claim_pending_fetch(batch_size) → 最多 N 行 feed_entries
     UPDATE feed_entries SET state='fetching', lease_owner=?, attempt_count += 1
     WHERE state='pending_fetch' AND (lease_expires_at IS NULL OR lease_expires_at < now)
     LIMIT N RETURNING *
  2. 对每行：
     a. HTTP GET → raw HTML（超时/大小边界）
     b. 永久抓取失败才懒读摘要并尝试 fallback
     c. 写入 raw_artifact（按 retention_policy）
     d. 执行配置的提取策略
     e. 策略链尝试 → ExtractedArticle 或 fallback
     f. 计算 content_hash，原子 insert-or-get
     g. 命中 → feed_entries='dedup_skipped'，关联 article_id
     h. 未命中 → INSERT articles，feed_entries='persisted'
     i. fallback 命中 → INSERT articles (quality='fallback')，feed_entries='fallback_persisted'
  3. 检查 max_batches，未达且仍有 pending → 继续；否则 break
```

### 5.1 batch_size 与 max_batches

- CLI `--batch-size`（ingest 默认50）：单批 claim 行数
- `app.runtime.max_batches_per_run`（默认 10）：单次 run 最多跑几批；`0` 表示不限
- 触达 max_batches → INFO 日志 + exit 0（**不**视为失败）

### 5.2 并发与 lease

claim SQL 用 `FOR UPDATE SKIP LOCKED`（PG）或单条 UPDATE...RETURNING 的数据库写锁（SQLite）保证并发安全。
lease 字段约束见 [./08-state-machines.md](./08-state-machines.md) §2.3 + [./05-storage.md](./05-storage.md)。

## 6. RawArtifact 留档（HTML）

依据 `config.artifact.retention_policy` 写入 `raw_artifacts` 表：

- 写入时机：`fetching → extracting` transition 之前（HTML 已抓到，未解析）
- 独立事务 commit（确保即使后续 extract 崩溃，artifact 仍在）
- `kind = 'html_payload'`
- `target_id = feed_entries.id`

详见 [./10-replay-and-backfill.md](./10-replay-and-backfill.md)。

## 7. replay --kind html

从 raw_artifact 重新跑策略链，对比与现有 article 的差异：

```bash
rss-ai-news replay --kind html --target-id <feed_entry_id> --diff
```

流程：
1. SELECT raw_artifacts WHERE kind='html_payload' AND target_id=?
2. 调用 `ReadabilityStrategy.extract(payload, meta)`
3. 与 `articles` 中现有正文 diff（按 normalized whitespace）
4. 输出差异 + 状态报告

**不**改写数据库（read-only 模式）。详见 [./10-replay-and-backfill.md](./10-replay-and-backfill.md)。

## 8. 失败路径速查

| 失败点 | 错误变体 | retryable | 处理 |
|---|---|---|---|
| HTML fetch 超时 | `ExtractorError::HttpTimeout` | true | 回 `pending_fetch` |
| HTML 4xx | `ExtractorError::HttpStatus { 4xx }` | false | 尝试 fallback（W18）；无摘要则 `failed` |
| HTML 5xx | `ExtractorError::HttpStatus { 5xx }` | true | 回 `pending_fetch` |
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
超时/5xx 保持跨 run 重试。HTTP 期间不持有数据库事务。
Feed/HTML 用有上限的分块 Vec 累计后直接移交，保留 Content-Length 与实际读取双重检查。
