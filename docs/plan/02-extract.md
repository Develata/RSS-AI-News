# 02 — 正文提取段

本章详解主链路第二段：从 `feed_entries.state=pending_fetch` 到 `articles` 入库。

```text
pending_fetch (claim+lease)
  → fetching   (HTML 抓取)
    → 抓取永久失败（4xx/too_large/invalid_url）→ summary fallback
      → 摘要可用 → fallback_persisted（W18，见 ./17-extract-fetch-fallback.md）
      → 无摘要   → failed
    → 抓取可重试失败（超时/5xx/429）→ 回 pending_fetch + next_attempt_at 冷却（W15 预算路径，§5.3）
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

### 2.2 字符集解码（2026-11-21）

HTML 客户端关闭自动 `Referer`（`.referer(false)`，R2-S11）：跨源重定向不会把文章 URL（含签名查询参数）带给另一主机，与 feed 客户端一致。
fetcher 把响应 `Content-Type` 原样放进 `RawHtmlFetch.content_type`。extract flow 在策略链前调用
`rss_ai_news_extractor::decode_html(bytes, content_type)`，按 WHATWG 编码嗅探顺序（简化）选候选：
BOM → `Content-Type` 的 `charset` 参数 → 前 1024 字节内的 `<meta charset>` /
`<meta http-equiv="Content-Type" content="…charset=…">`（按 WHATWG 预扫描逐个消费标签与属性：其它标签属性值里的
`<meta …>` 文本、引号内的 `>` 都不会被误读；跳过注释与 `<!…>` / `</…>` / `<?…>`；声明到第 1024 字节仍未结束则作废；
meta 声明的 UTF-16 视为 UTF-8、`x-user-defined` 视为 windows-1252；响应头参数按带引号字符串解析，其它参数引号内的
`charset=` 不算，R2-S8）→ UTF-8。
每个候选**严格**解码（`encoding_rs`，不插 U+FFFD），失败则试下一个；全部失败为
`ParseFailed`（走 fallback / `failed`）。与浏览器不同，不按 locale 猜 windows-1252：未声明且非
UTF-8 的字节维持旧行为失败，避免发布乱码。策略收到的是 UTF-8；`raw_artifacts` 保留原始字节。
`ReadabilityStrategy` 收到非 UTF-8 字节时（如 replay 传入原始 artifact）按 BOM / `<meta>` 自行解码；
只由响应头声明字符集的页面，其 artifact 不含 Content-Type，replay 无法还原该声明。

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

字数（`word_count`，Readability 分级与 `articles.word_count` 共用，`strategy::word_count`）：
`High` 需 ≥ 300 词且有标题，`Medium` 需 ≥ 100 词，不足为 `ContentTooShort`（其 `chars`
字段报告字符数而非字节数）。不含 CJK 字符的空白分隔 token 记 1 词（与旧
`split_whitespace` 计数完全一致）；含 CJK 的 token 内，每段含字母或数字的非 CJK 连续片段记
1 词，CJK 字符（汉字、平假名、片假名、谚文——Unicode 17 `Scripts.txt` 中 Han / Hiragana / Katakana / Hangul 的全部区段，
含 `〇`、部首、扩展 J、假名补充 / 扩展 / 小假名区与带圈 / 半角形式，外加原有的 `ー`、`・` 等假名标点，R2-B4）合计 `⌊n / 2⌋` 词。中英平行文本约 1.6 个汉字对应
1 个英文词，取 2 偏保守、并向下取整，故 200 个汉字起为 `Medium`、600 个起为 `High`
（2026-11-21，修复中文长文被判过短）。韩文本身以空格分词，此规则对韩文约高估 1.5 倍。
存量 article 的 `word_count` 不回写；`replay --diff` 对旧 CJK 文章会显示字数差异。

## 4. 第三层去重：content_hash

正文提取使用 SHA-256 计算 content_hash，storage::insert_or_get_by_content_hash
依靠数据库唯一约束原子插入或返回已有 article，禁止 SELECT-exists 再 INSERT。
命中后 feed_entry 进入 dedup_skipped 并关联已有 article；schema/claim 状态由 storage 负责。

原子性（2026-11-21）：extract 调 `ArticleRepository::persist_for_entry`，在**一个事务**内完成
insert-or-get article 与 owner-guarded 的 entry 推进（persisted / fallback_persisted / dedup_skipped）；
owner guard 落空则整体回滚（不留孤儿 article），报 `LeaseLost`。命中的已有 article 若
`origin_feed_entry_id` 就是本条目（旧版本在 article 提交后、release 前崩溃留下的），记为本条目的
`persisted`（`newly_created = false`），不再误判为 `dedup_skipped` 指向自己。
恢复优先（R2-S6）：同一事务内**先**按 `origin_feed_entry_id` 查本条目已拥有的 article（`idx_articles_origin_feed_entry`，
migration 0009）。恰有一篇时直接复用它，**不**插入本次提取的正文（重试可能得到不同正文哈希 H2，旧做法会再建一篇，
使同一 entry 拥有两篇 article、task_gen 两篇都处理），entry 状态按已存 article 推导（`extractor_strategy =
summary_fallback` → `fallback_persisted`，否则 `persisted`），与本次是否走摘要路径无关；`EntryPersistOutcome::Persisted.fallback`
报告实际落库的状态，运行摘要计数（`persisted` / `fallback_persisted`）与 transition 日志据此生成（R3E-S6）。拥有多篇时无法判定，
返回 `StorageError::Integrity`（不可重试，事务回滚，entry 不动），由 extract 按存储故障如实上报。SQLite 该事务改用
`BEGIN IMMEDIATE`（先读后写，避免升级写锁时 busy）。

命中时已有 article 的字段保持不变（先写者为准，`ON CONFLICT DO NOTHING`）：这次提取新写入的 html artifact
**不会**补到已有 article 的 `body_html_artifact_id`。已有 article 可能本来就没有 artifact（`on_failure` 下提取成功、
`off`、采样未命中、写入失败，或被 TTL 清理后置 NULL），即便如此也不补挂。新 artifact 以 feed entry id 为 key，
写入成功且未被清理时，`replay --kind html --key <feed_entry_id>` 仍可回放，`--diff` 经 `feed_entries.article_id`
找到对应 article（R3 审查遗留，2026-11-16 定为既定语义）。

### 4.0 extractor_version（2026-11-21）

`articles.extractor_version` 引用 `rule_versions` 中 `kind='extractor'` 的行：CLI 装配 extract 时
（`context_factory::resolve_extractor_version`）经 `active_rule_or_register("extractor", EXTRACTOR_RULE_TAG, …)`
取 active 行，首次使用时登记（tag 见 `rss_ai_news_extractor::EXTRACTOR_RULE_TAG`）。此前两条写入路径写字面量
`1`，在新库上指向 `kind='config'` 行。历史 article 行**不回写**，其 `extractor_version` 仍为旧值，审计时需按
`rule_versions.kind` 区分。改变提取规则时提升 tag；同 kind 的 active 轮换未实现（与 config 的 sha 轮换不同），
已有 active extractor 行时新 tag 不会自动生效。

### 4.1 与一/二层的区别

- 一/二层在 INSERT 前发生，**不**产生新 `feed_entries` 行
- 三层发生在 `extracting` 阶段，**已有 feed_entries 新行**，所以产生真实的 transition

## 5. Extract Flow

由 `ExtractFlow` 在 [`crates/runtime/src/flows/extract.rs`](../../crates/runtime/src/flows/extract.rs) 编排：

```text
excluded = {}                               -- 本 run 已领取、未确认终态成功的 id
loop:
  1. claim_pending_fetch(batch_size, excluded) → 最多 N 行 feed_entries
     UPDATE feed_entries SET state='fetching', lease_owner=?, attempt_count += 1
     WHERE state='pending_fetch' AND (lease_expires_at IS NULL OR lease_expires_at < now)
       AND (next_attempt_at IS NULL OR next_attempt_at <= now)
       AND id NOT IN excluded
       AND source_id IN (SELECT id FROM feed_sources
                         WHERE status='active' AND (:category IS NULL OR category_key=:category))
     ORDER BY discovered_at ASC LIMIT N RETURNING *
  2. 对每行：
     a. HTTP GET → raw HTML（超时/大小边界）
     b. 永久抓取失败才懒读摘要并尝试 fallback
     c. `always` / `sampled`：写入 raw_artifact
     d. 执行配置的提取策略；`on_failure` 且无策略成功时才写 raw_artifact
     e. 策略链尝试 → ExtractedArticle 或 fallback
     f. 计算 content_hash；同一事务内 insert-or-get + owner-guarded entry 推进（§4）
     g. 命中他人 article → feed_entries='dedup_skipped'，关联 article_id；命中自己的 → 'persisted'
     h. 未命中 → INSERT articles，feed_entries='persisted'
     i. fallback 命中 → INSERT articles (quality='fallback')，feed_entries='fallback_persisted'
  3. claim 为空 → break；达到 max_batches → break；否则继续
     （retryable 失败**不**终止循环，见 §5.3）
```

claim 只领取 `status='active'` 源的条目；`ingest --category X` 时只领取分类 X 的条目（2026-11-21，此前
extract 不看分类与源状态，会抓取并改写其他分类及暂停源的积压）。暂停 / 归档源的待抓条目保持
`pending_fetch`，源恢复 active 后照常领取。

### 5.1 batch_size 与 max_batches

- CLI `--batch-size`（ingest 默认50）：单批 claim 行数
- `app.runtime.max_batches_per_run`（默认 10）：单次 run 最多跑几批；`0` 表示不限
- 触达 max_batches → INFO 日志 + exit 0（**不**视为失败）

### 5.2 并发与 lease

lease 预算（2026-11-21）：一批 claim 共用一个 `lease.fetch_duration_seconds`，按
`http.concurrent_fetches` 分 `ceil(batch / concurrent)` 波处理，每波最多 `http.timeout_seconds`
加 `EXTRACT_WAVE_MARGIN_SECONDS = 5` 秒解析 / 写库余量。`ingest`（及 `run` 的 ingest 段）在 extract 前计算
`ceil(batch / concurrent) × (timeout + 5)`；超过 lease 时**不拒绝启动**，而是把本次批大小缩到能放进 lease
的整波数（`fit_extract_batch_to_lease`）并写 WARN。连一波都放不下时（`timeout + 5 > lease`）本次**不领取**
extract（任何一批都可能在处理中途被另一 run 回收），ingest 照常运行，命令报 `extract_lease_too_short` 运行错误
并退出 1（R2-S9，旧做法仍按一波领取）。这样升级前能运行的配置（如 lease 300 s + 默认批 50 → 本次按 40）继续运行，
且一批不会超出自己的 lease。`+5` 秒是解析 / 写库耗时的估算余量，不是数据库与解析时间的硬上界。
示例配置的 lease 改为 600 s，默认批大小不再被缩小。

滚动 JoinSet 将存活任务数限制在 `http.concurrent_fetches.max(1)`，不为整个 claim 批次预建等待任务。
claim 行集合仍占 O(batch size) 内存；CLI 批大小范围见 [09-cli-and-runtime](./09-cli-and-runtime.md)。

claim SQL 用 `FOR UPDATE SKIP LOCKED`（PG）或单条 UPDATE...RETURNING 的数据库写锁（SQLite）保证并发安全。
lease 字段约束见 [./08-state-machines.md](./08-state-machines.md) §2.3 + [./05-storage.md](./05-storage.md)。

### 5.3 retryable 失败冷却（2026-11-20）

- retryable 失败（超时 / 5xx / 连接失败，且预算未尽）回 `pending_fetch`，并在同一条
  owner-guarded release UPDATE 内写 `next_attempt_at = release 时刻 + backoff`
  （时刻在 release 前采样，策略链耗时不缩短冷却；存为 UTC 并向上取整到毫秒）。
- backoff 按刚失败的 attempt 有界指数增长：10 / 20 / 40 / 80 min，上限 2 h
  （`RETRY_BACKOFF_BASE` / `RETRY_BACKOFF_CAP`，`flows/extract.rs`）。
- 每次 claim 按该次 claim 的时刻判断是否到期：冷却行在 `LIMIT` 前被过滤、不占批次名额；
  run 期间到期的行（包括上一 run 留下的冷却）在同一 run 内即可被领取。
- 本 run 尝试过的行不会被本 run 再次领取：claim 排除本 run 已领取、且未确认终态成功
  （persisted / fallback_persisted / dedup_skipped 的 release 已落库）的 id。release 落空报 `LeaseLost`、
  release 报错报 `ReleaseFailed`（计入 `storage_failed`），二者都不冒充已落库的转移。失败行即使在
  run 结束前冷却到期、或 release 落空（lease 过期被并发 run reclaim，owner guard 拦下
  release，行回到 `pending_fetch` 且无冷却）、或被并发 backfill reset，也留给之后的 run。
  排除集合只随本 run 的失败数增长。跨 run 的过期 lease 恢复不变。
- 冷却比较从不提前：截止点存为 UTC 并向上取整到毫秒，claim 的比较时刻向下取整到毫秒
  （最多晚约 2 ms）。SQLite 以小数位数可变的 RFC3339 TEXT 存时间，同一秒内字典序 ≠
  时刻序，故用 `julianday()`（舍入到毫秒，两侧已对齐故精确）；`julianday()` 解析不了的
  文本回退到文本比较。PG 直接比较 `TIMESTAMPTZ`。"从不提前"针对本版本写入的截止点。
- 排除集合按本 run 的失败数增长；失败密集且 `batch_size = 1`、不限批时，累计发送的 id
  约为 F(F−1)/2（F 为本 run 失败数）。正常规模（每批 50 × 10 批）可忽略，未做非空列表实测。
- 循环继续领取其余可领取行，直到 claim 为空或达到 `max_batches`。`retryable_deferred`
  表示本 run 有 retryable 失败、本 run 不再尝试这些行（通常已带冷却；release 落空时
  不带冷却，由其他 run 处理），可与 `max_batches_reached` 同时为 true。
- extract 的时间来自 `ExtractDeps::clock`（生产为系统时钟），测试注入可推进的时钟。
- 预算、最后一次尝试折叠为 `failed`、`terminalize_exhausted` sweep、过期 lease reclaim、
  永久失败与 summary fallback 均不变；backfill reset 清空 `next_attempt_at`。
- claim 仍按 `discovered_at ASC`：可领取的旧积压大于 `batch_size × max_batches` 时，新条目
  仍可能整轮排不上（`oldest_first_backlog_still_starves_fresh_entries_under_cap` 记录此限制）。
  冷却只保证一篇失败文章不再阻断整轮，不保证新文章能进入发布窗口。

**提案（未实现）：有界 fresh-first claim。** 把可领取行分成两类：fresh =
`now - 发布窗口 <= COALESCE(published_at, discovered_at) <= now`（与发布阶段
`SELECT_AI_PATH_CANDIDATES_SQL` 的窗口两端同口径；`published_at` 可为空故回退 `discovered_at`；
源站给出未来时间戳的条目不占 fresh 名额，归入 backlog），其余为 backlog。每批 N 行中至少保留
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
| HTML 429 | `ExtractorError::RateLimited { retry_at }`（`http_429`） | true | 回 `pending_fetch` + 冷却（§5.3），不消费摘要；`next_attempt_at = max(释放时刻 + 本地退避, retry_at)`。`retry_at` 是**收到响应时**确定的绝对时刻（R3E-S10）：秒数形式 = 收包时刻 + 秒数，HTTP-date（三种 RFC 9110 格式）= 该日期本身（过去的日期即不额外等待）；释放晚于收包不会平移它。上限：`retry_at ≤ 收包时刻 + 1 天`（`MAX_RETRY_AFTER_SECONDS`，防止异常服务器无限期停放条目）——合法的更长等待（如 48 h）因此会被截到 1 天，提前的重试可能再遇 429 并消耗一次重试预算（已知取舍）。缺失 / 无效则只用本地退避（R2-S10） |
| HTML 其余 4xx | `ExtractorError::HttpStatus { 4xx }` | false | 尝试 fallback（W18）；无摘要则 `failed` |
| HTML 5xx | `ExtractorError::HttpStatus { 5xx }` | true | 回 `pending_fetch` + 冷却（§5.3） |
| payload 过大 | `ExtractorError::TooLarge` | false | 尝试 fallback（W18）；无摘要则 `failed` |
| 提取失败 | `ExtractorError::ParseFailed` | false | 尝试 fallback；失败则 `failed` |
| 内容太短 | `ExtractorError::ContentTooShort` | false | 尝试 fallback；失败则 `failed` |
| article 写入 / 摘要回读的存储错误 | `StorageError`（`is_retryable()`） | 按分类 | 可重试（池超时、PG 40001/40P01、SQLite busy）→ 回 `pending_fetch` + 冷却，耗尽折叠 `failed`，`last_error_kind` 为真实存储错误；不可重试 → `failed`。摘要回读失败不再当作"无摘要" |
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
