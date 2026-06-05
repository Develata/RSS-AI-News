# 01 — Feed 抓取段

本章详解主链路第一段：从 `FeedSource` 到 `FeedEntry` 入库。

```text
FeedSource → HTTP fetch → FeedResponse → parser → FeedEntryMeta → 三层去重 → INSERT feed_entries
                                                                ↓
                                                            生成 ArticleFetchTask（移交 02 章）
```

## 1. 边界

本章覆盖：
- feed 源配置（FeedSource）
- HTTP 抓取（含条件请求）
- RSS / Atom / JSON Feed / RSSHub 解析
- 一层（UID）+ 二层（link_hash）去重
- `feed_entries` INSERT 与状态推进到 `Discovered → PendingFetch`

**不覆盖**：
- 详情页 HTML 抓取与正文提取 → [./02-extract.md](./02-extract.md)
- 第三层（content_hash）去重 → [./02-extract.md](./02-extract.md)
- AI 任务生成 → [./03-ai.md](./03-ai.md)

## 2. FeedSource 配置

`FeedSource` 来自 `categories/<key>.toml` 的 `[[sources]]` 数组。每个分类下可配置多个源。

```toml
[[sources]]
key = "openai-blog"                   # category 内唯一
display_name = "OpenAI Blog"
feed_url = "https://openai.com/blog/rss.xml"
feed_kind = "rss"                      # rss | atom | json_feed | rsshub
priority = 10                          # 排序与显示优先级
enabled = true
```

加载后写入 `feed_sources` 表，作为状态机的真相源。配置详见 [./06-config.md](./06-config.md)。

### 2.1 RSSHub 占位符

`feed_url` 可包含 `{RSSHUB}` 或 `{RSSHUB_BASE_URL}` 占位符；运行时由 `.env` 中
`RSSHUB_BASE_URL` 替换。`RSSHUB_ACCESS_KEY` 自动加为 `?key=...` 查询参数。

占位符约束：
- 配置 schema **不**接受裸 URL 中的密钥（避免持久化泄露）
- 占位符展开仅在运行时进行，**不**写回 `feed_sources.feed_url`
- 详见 [../adr/0007-rsshub-secret-runtime-expansion.md](../adr/0007-rsshub-secret-runtime-expansion.md)

## 3. HTTP 抓取

`FeedFetcher` trait 在 [`crates/feed/src/fetcher.rs`](../../crates/feed/src/fetcher.rs) 定义。
唯一生产实现是 `reqwest`-based HttpFeedFetcher，单进程共享一个 Client。

### 3.1 条件请求

每次抓取自动携带（来自 `feed_sources` 上次响应）：
- `If-None-Match: <etag>`
- `If-Modified-Since: <last_modified>`

返回 304 = 跳过本次抓取，状态机 transition 数为 0。

### 3.2 超时与重试

- 单请求超时：`runtime.http_timeout_seconds`（默认 30s）
- 连接超时：5s
- TCP keep-alive：开
- 失败分类：见 [./11-error-and-recovery.md](./11-error-and-recovery.md) `FeedError`

retryable 错误：超时、5xx、连接失败 → 状态保持，下轮重试
非 retryable：4xx（除 408 / 429）、过大、解析失败 → 直接转 `Failed`

### 3.3 代理与 TLS

代理由 `app.toml` 全局 `[http].proxy` 配置；TLS 通过 system CA bundle 验证（镜像内
`ca-certificates` 包必须存在，见 [./12-deployment.md](./12-deployment.md)）。

## 4. Parser

`FeedParser` trait 在 [`crates/feed/src/parser.rs`](../../crates/feed/src/parser.rs) 定义。
按 `FeedKind` 派遣到具体实现。

### 4.1 各 kind 的实现

| FeedKind | 库 | 备注 |
|---|---|---|
| `Rss` | `rss` crate | 支持 RSS 2.0 |
| `Atom` | `atom_syndication` | Atom 1.0 |
| `JsonFeed` | 自实现 `serde_json` parser | jsonfeed.org 1.1 |
| `RssHub` | 走 RSS 分支 | RSSHub 输出始终是 RSS 2.0 |

### 4.2 FeedEntryMeta 规范化

所有 parser 输出统一的 `FeedEntryMeta` DTO（见 `crates/domain/src/dto/`）：

```rust
struct FeedEntryMeta {
    feed_entry_uid: String,        // 源 ID（GUID / link / 哈希）
    title: String,
    link: Option<String>,
    summary_raw: Option<String>,
    published_at: Option<OffsetDateTime>,
    raw_categories: Vec<String>,
}
```

关键：`link` **规范化**（小写 host、去 fragment、按 query 白名单过滤跟踪参数），结果存
`feed_entries.normalized_link`，用作第二层去重。`link_hash = blake3(normalized_link)`。

### 4.3 published_at 时间窗口过滤

`app.publish.candidate_window_days`（默认 7）控制候选时间窗口：早于窗口的条目**不**进入
`feed_entries`，仅向 tracing 输出 `entry_out_of_window` 事件。这是 v0.2 引入的能力。

## 5. 三层去重

详细规则在 [./08-state-machines.md](./08-state-machines.md) §3.2。本章只列 ingest 阶段覆盖的两层：

### 5.1 第一层：UID

`UNIQUE(source_id, feed_entry_uid)` SQL 约束在 INSERT 时拦截重复 GUID。命中**不产生新行**，
runtime 聚合一条 `entry_dedup_skipped` event。

### 5.2 第二层：normalized_link

INSERT 前 SELECT `feed_entries WHERE link_hash = ? AND source_id != ?`（跨源去重）。命中**不产生
新行**，聚合到同一条 dedup event。

第三层 `content_hash` 在正文入库时判定，详见 [./02-extract.md](./02-extract.md)。

## 6. Ingest Flow

由 `IngestFlow` 在 [`crates/runtime/src/flows/ingest.rs`](../../crates/runtime/src/flows/ingest.rs) 编排：

```text
1. 加载 active feed_sources（过滤 --category）
2. 并发抓取（concurrency = app.feed.concurrent_feeds，默认 4）
3. 每个源：
   a. HTTP fetch with conditional request
   b. parse → FeedEntryMeta 列表
   c. 对每个 entry：
      - 一层 UID check（INSERT ON CONFLICT DO NOTHING）
      - 二层 link_hash check（SELECT）
      - 通过 → INSERT feed_entries (state='discovered') 然后 UPDATE state='pending_fetch'
4. 聚合 run_events，更新 feed_sources.last_fetched_at / etag / last_modified
5. （可选）继续到 extract 阶段（默认开启，--skip-fetch 关闭）
```

### 6.1 并发与批次

- feed 抓取并发度由 `app.feed.concurrent_feeds` 控制
- 每个 source 内串行处理 entry（避免对同源批量并发）
- ingest **不**进入 extract 的 batch 循环；extract 由 `--max-batches` 控制（见 [./02-extract.md](./02-extract.md)）

## 7. RawArtifact 留档

依据 `config.artifact.retention_policy`：

| policy | feed payload 行为 |
|---|---|
| `off` | 不写 |
| `on_failure` | 解析前写入；解析成功后同事务清理；失败时保留 |
| `always` | 始终写入并保留 |

写入路径：`crates/runtime/src/artifact.rs::ArtifactWriter`。详见 [./10-replay-and-backfill.md](./10-replay-and-backfill.md)。

## 8. 失败路径速查

| 失败点 | 错误变体 | retryable | 处理 |
|---|---|---|---|
| HTTP 超时 | `FeedError::HttpTimeout` | true | source 跳过，下次再试 |
| 4xx | `FeedError::HttpStatus { 4xx }` | false | source 仍 active，但本次跳过 |
| 5xx | `FeedError::HttpStatus { 5xx }` | true | 同超时 |
| 过大 (`> max_payload_bytes`) | `FeedError::TooLarge` | false | source 跳过 |
| 解析失败 | `FeedError::ParseFailed` | false | source 跳过 |

**注意**：feed 抓取失败**不**导致 source 转 paused / archived；source 状态由人工管理。
失败信息写 `feed_sources.last_error*`，可被 `doctor` 看到。

## 9. 当前实现入口

| 内容 | 路径 |
|---|---|
| Flow 编排 | [`crates/runtime/src/flows/ingest.rs`](../../crates/runtime/src/flows/ingest.rs) |
| FeedFetcher trait + 实现 | [`crates/feed/src/fetcher.rs`](../../crates/feed/src/fetcher.rs) |
| FeedParser trait + 实现 | [`crates/feed/src/parser.rs`](../../crates/feed/src/parser.rs) |
| FeedError | [`crates/feed/src/error.rs`](../../crates/feed/src/error.rs) |
| FeedEntryRepository | [`crates/storage/src/repo/feed_entry.rs`](../../crates/storage/src/repo/feed_entry.rs) |
| 集成测试 | [`crates/runtime/tests/ingest_tests.rs`](../../crates/runtime/tests/ingest_tests.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md)登记漂移。
