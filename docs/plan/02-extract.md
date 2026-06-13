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

每次抓取强制：
- HEAD-then-GET：先 HEAD 检查 `Content-Type` 与 `Content-Length`，过滤非 HTML / 超大
- 单请求超时：`runtime.html_timeout_seconds`（默认 30s）
- 最大 payload：`extractor.max_content_size_bytes`（默认 5 MB）
- User-Agent：固定字符串（绕过部分反爬）
- 自动跟随 redirect（最多 5 次）

### 2.2 媒体类型过滤

允许的 `Content-Type` 白名单：
- `text/html`（含 charset 变体）
- `application/xhtml+xml`

其它（如 `application/pdf` / `video/*` / `image/*`）直接 `ExtractorError::UnsupportedMediaType` → `failed`。

## 3. 策略链

`ContentStrategy` trait 在 [`crates/extractor/src/strategy.rs`](../../crates/extractor/src/strategy.rs) 定义。
当前策略链有 2 个，按顺序尝试：

| 顺序 | 策略 | 实现 |
|---|---|---|
| 1 | Readability | `readability` crate 适配，基于 Mozilla Readability 算法 |
| 2 | SummaryFallback | 使用 `feed_entries.summary_raw` 作为正文，标记 `content_quality='fallback'` |

每个策略的契约：
- 输入：原始 HTML payload + `FeedEntryMeta`
- 输出：`Result<ExtractedArticle, ExtractorError>`
- `ExtractedArticle` 包含：正文 text、HTML、quality 标记

成功条件：策略返回 `Ok` 且内容长度 ≥ `extractor.min_content_length`（默认 200 字符）。
内容太短 → `ExtractorError::ContentTooShort` → 尝试下一个策略。

### 3.1 content_quality 分级

| Quality | 来源 | 处理 |
|---|---|---|
| `High` | Readability 命中且字数充足 | 正常入库 |
| `Medium` | Readability 命中但字数 borderline | 正常入库 |
| `Fallback` | summary fallback 兜底 | 入库为 `fallback_persisted` |

quality 由 strategy 自己决定。AI 阶段会跳过 `Fallback` 行（避免分析 summary），见 [./03-ai.md](./03-ai.md)。

## 4. 第三层去重：content_hash

正文提取成功后，在 `articles` INSERT 之前：

```sql
SELECT id FROM articles WHERE content_hash = ?
```

`content_hash` 由 `crates/domain/src/link_normalizer.rs` 计算（基于规范化正文 BLAKE3）。
命中 → **不**插入 articles，`feed_entries` 转 `DedupSkipped`（`dedup_decision='hash_dup'`）+ 关联到已有 article。

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
     a. HTTP HEAD → 校验
     b. HTTP GET → raw HTML
     c. 写入 raw_artifact（按 retention_policy）
     d. state='extracting'
     e. 策略链尝试 → ExtractedArticle 或 fallback
     f. 计算 content_hash，三层 dedup check
     g. 命中 → feed_entries='dedup_skipped'，关联 article_id
     h. 未命中 → INSERT articles，feed_entries='persisted'
     i. fallback 命中 → INSERT articles (quality='fallback')，feed_entries='fallback_persisted'
  3. 检查 max_batches，未达且仍有 pending → 继续；否则 break
```

### 5.1 batch_size 与 max_batches

- `app.runtime.batch_size`（默认 50）：单批 claim 行数
- `app.runtime.max_batches_per_run`（默认 10）：单次 run 最多跑几批；`0` 表示不限
- 触达 max_batches → INFO 日志 + exit 0（**不**视为失败）

### 5.2 并发与 lease

claim SQL 用 `FOR UPDATE SKIP LOCKED`（PG）或 `BEGIN IMMEDIATE`（SQLite）保证并发安全。
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
| 不支持的媒体类型 | `ExtractorError::UnsupportedMediaType` | false | 转 `failed` |
| payload 过大 | `ExtractorError::TooLarge` | false | 尝试 fallback（W18）；无摘要则 `failed` |
| 提取失败 | `ExtractorError::ParseFailed` | false | 尝试 fallback；失败则 `failed` |
| 内容太短 | `ExtractorError::ContentTooShort` | false | 尝试 fallback；失败则 `failed` |
| content_hash 命中 | 非错误 | — | 转 `dedup_skipped` |
| fallback 成功 | 非错误 | — | 转 `fallback_persisted` |

## 9. 配置关键项

参考 [./06-config.md](./06-config.md)：

```toml
[extractor]
max_content_size_bytes = 5_242_880    # 5 MB
min_content_length = 200
http_timeout_seconds = 30
allowed_content_types = ["text/html", "application/xhtml+xml"]
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
