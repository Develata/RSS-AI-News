# 内部 DTO 契约

## 1. 定位

本文档定义 Rust 版各 crate 之间传递数据的中间结构体（DTO）。DTO 不是数据库行对象，也不是领域对象——它们是层间通信的短生命周期载体。

核心区分：

| 概念 | 定义 | 所在 crate |
|---|---|---|
| 领域对象 | 持久化真相源的内存映射（如 `Article`） | `domain` |
| 数据库行 | `sqlx::FromRow` 映射的 raw 行 | `storage` |
| **DTO** | 层间传递的短生命周期结构体 | `domain`（定义）/ 各 crate（使用）|

原则：

- DTO 定义在 `domain` crate 的 `dto` 模块中
- DTO 使用 `#[derive(Clone, Debug)]`，不使用 `serde::Serialize`（除非有外部输出需求）
- DTO 字段全部 owned，不使用生命周期参数（避免跨层传递时的借用复杂度）
- DTO 是不可变的：创建后不修改

## 2. Feed 阶段 DTO

### 2.1 `FeedFetchRequest`

`runtime` → `feed`：发起 feed 拉取的请求。

```text
FeedFetchRequest
├── source_id: i64
├── category_key: String
├── source_key: String
├── feed_url: String                    # 已替换 {RSSHUB} 后的完整 URL
├── feed_kind: FeedKind
├── etag: Option<String>
├── last_modified: Option<String>
├── timeout: Duration
```

### 2.2 `FeedFetchResponse`

`feed` → `runtime`：拉取结果。

```text
FeedFetchResponse
├── source_id: i64
├── http_status: u16
├── etag: Option<String>                # 新的 ETag
├── last_modified: Option<String>       # 新的 Last-Modified
├── not_modified: bool                  # 304 时为 true
├── entries: Vec<FeedEntryMeta>
├── raw_payload_bytes: Option<Vec<u8>>  # 供 raw_artifact 存储
```

### 2.3 `FeedEntryMeta`

`feed` → `runtime`：从 feed 解析出的单条条目元数据。

```text
FeedEntryMeta
├── feed_entry_uid: String              # feed 内原始 guid/id
├── title_raw: String
├── link_raw: String                    # 未规范化的原始链接
├── summary_raw: Option<String>
├── published_at: Option<OffsetDateTime>
```

### 2.4 `DedupResult`

`runtime` 内部：去重判定结果。

```text
DedupResult
├── entry_meta: FeedEntryMeta
├── normalized_link: String
├── link_hash: String
├── decision: DedupDecision             # enum { Fresh, UidDup, LinkDup, HashDup }
├── existing_entry_id: Option<i64>      # 如果是 dup，指向已有行
```

## 3. 正文提取阶段 DTO

### 3.1 `ArticleFetchTask`

`runtime` → `extractor`：正文抓取任务。

```text
ArticleFetchTask
├── feed_entry_id: i64
├── normalized_link: String
├── title_raw: String
├── summary_raw: Option<String>
├── timeout: Duration
```

### 3.2 `ExtractedArticle`

`extractor` → `runtime`：提取结果。

```text
ExtractedArticle
├── feed_entry_id: i64
├── canonical_link: String
├── title: String                       # 清洗后标题
├── body_text: String                   # 纯文本正文
├── body_html: Option<Vec<u8>>          # 原始 HTML（供 artifact 存储）
├── extractor_strategy: ExtractorStrategy  # enum { Readability, Rule, SummaryFallback }
├── content_quality: ContentQuality     # enum { High, Medium, Fallback }
├── word_count: u32
├── content_hash: String                # body_text 的 SHA-256
```

### 3.3 `FallbackArticle`

`extractor` → `runtime`：正文提取失败后的降级结果（使用 feed summary）。

```text
FallbackArticle
├── feed_entry_id: i64
├── canonical_link: String
├── title: String
├── body_text: String                   # 来自 summary_raw
├── content_quality: ContentQuality     # 固定为 Fallback
├── word_count: u32
├── content_hash: String
```

## 4. AI 阶段 DTO

### 4.1 `AiTask`

`runtime` → `ai`：AI 处理任务。

```text
AiTask
├── article_ai_result_id: i64
├── article_id: i64
├── title: String
├── body_text: String                   # 已截断到 max_input_chars
├── category_key: String
├── prompt_template: String
├── model_id: String
├── max_tokens: u32
├── temperature: f32
```

### 4.2 `AiOutput`

`ai` → `runtime`：AI 解析后的结构化输出。

```text
AiOutput
├── article_ai_result_id: i64
├── summary: String
├── tags: Vec<String>
├── importance_score: Score0To100        # 0–100，newtype（见 §6.5）
├── keep_decision: bool
├── raw_response: String                # 原始 AI 响应文本（供 artifact 存储）
```

### 4.3 `AiFilteredOutput`

`ai` → `runtime`：AI 判定为不值得发布。

```text
AiFilteredOutput
├── article_ai_result_id: i64
├── reason: String                      # AI 给出的过滤理由
├── raw_response: String
```

## 5. 发布阶段 DTO

### 5.1 `PublishRequest`

`runtime` → `report`：发起发布的请求。

```text
PublishRequest
├── category_key: String
├── report_date: String                 # YYYY-MM-DD
├── target_timezone: String             # IANA tz
├── render_version_id: i64
├── selection_policy_version_id: i64
├── max_items: u32
├── min_importance_score: Score0To100   # 见 §6.5
├── include_unscored: bool
```

### 5.2 `PublishCandidate`

`report` 内部 / `storage` → `report`：候选发布项。**`article_ai_result_id` 与 `importance_score` 必须同时为 `Some` 或同时为 `None`**，由 `try_new` 校验；其语义来源于发布路径分叉，详见 [state-machine §4.1.3](./state-machine.md#413-ai-关闭--无-ai-发布降级)。

```text
PublishCandidate
├── article_id: i64
├── article_ai_result_id: Option<i64>
├── title: String
├── canonical_link: String
├── summary: String
├── tags: Vec<String>
├── importance_score: Option<Score0To100>   # 与 article_ai_result_id 同 Some/同 None
├── source_display_name: String
├── category_key: String
├── published_at: Option<OffsetDateTime>
```

### 5.3 `FrozenPublishItem`

`report` → `storage`：冻结后的发布项，直接映射到 [publish_items](./storage-schema.md#46-publish_items) 行；同 NULL 约束由 schema CHECK 保证。

```text
FrozenPublishItem
├── position: u32
├── article_id: i64
├── article_ai_result_id: Option<i64>       # 与 frozen_score 绑定：同生同灭
├── frozen_title: String
├── frozen_summary: String
├── frozen_tags_json: String                # "[]" when AI off
├── frozen_score: Option<Score0To100>       # 与 article_ai_result_id 绑定（见 §6.5）
├── frozen_canonical_link: String
├── frozen_source_display_name: String
```

### 5.4 `RenderedReport`

`report` → `publish`：渲染完成的报告。

```text
RenderedReport
├── publish_record_id: i64
├── category_key: String
├── report_date: String
├── markdown_content: String
├── relative_path: String               # 如 "archive/ai/2025-01-15.md"
```

### 5.5 `PublishOutcome`

`publish` → `runtime`：发布结果。

```text
PublishOutcome
├── publish_record_id: i64
├── local_path: Option<String>
├── commit_sha: Option<String>
├── remote_target: Option<String>
```

## 6. Replay / Backfill DTO

### 6.1 `ReplayRequest`

`cli` → `runtime`：回放请求。

```text
ReplayRequest
├── artifact_kind: ArtifactKind         # enum { FeedPayload, HtmlPayload, AiRawResponse }
├── artifact_key: Option<String>        # 指定 key，或
├── artifact_id: Option<i64>            # 指定 id
├── dry_run: bool
```

### 6.2 `ReplayResult`

`runtime` → `cli`：回放结果。

```text
ReplayResult
├── artifact_kind: ArtifactKind
├── artifact_key: String
├── parsed_output: String               # 重新解析后的人类可读输出
├── diff: Option<String>                # 与当前数据库状态的差异（如有）
├── errors: Vec<String>                 # 回放过程中的错误
```

### 6.3 `BackfillRequest`

`cli` → `runtime`：补跑请求。

```text
BackfillRequest
├── target: BackfillTarget              # enum { Extract, Ai }
├── category_filter: Option<String>
├── date_range: Option<(String, String)>
├── batch_size: u32
├── dry_run: bool
├── prompt_version_id: Option<i64>      # target=Ai：覆盖 prompt 版本（默认使用当前活跃版本）
├── output_schema_version_id: Option<i64> # target=Ai：覆盖输出 schema 版本
├── model_id: Option<String>            # target=Ai：覆盖模型
├── extractor_version_id: Option<i64>   # target=Extract：覆盖提取器版本
```

版本字段语义：

- 留空 → 使用 `app.toml` 配置的当前活跃版本；运行时自动 bump 版本号写入 `rule_versions` 表
- 指定具体 id → 使用历史版本（用于复现某次历史结果），不再 bump，只复用已有版本行
- `target=Ai` 只消费 AI 相关版本字段，`target=Extract` 只消费 extractor 版本字段；错配的字段组合应在 `cli` 层拒绝

## 6.4 受约束 newtype（值域）

部分语义带值域的字段在 `domain` crate 用 newtype 包装，避免 `u8` 0–255 误差范围流入业务逻辑：

```text
Score0To100(u8)
├── 构造函数：try_new(value: u8) -> Result<Self, ScoreOutOfRange>
├── 仅当 value <= 100 才返回 Ok
├── Deref<Target = u8> 提供只读访问
├── serde：透明序列化为整数（不嵌套 newtype）
├── sqlx：Decode/Encode for INTEGER；Decode 失败时报 ScoreOutOfRange
```

| 使用位置 | 字段 | DB 列 | DB 约束 |
|---|---|---|---|
| `AiOutput.importance_score` | `Score0To100` | `article_ai_results.importance_score` | 见 [storage-schema §4.4](./storage-schema.md#44-article_ai_results) |
| `PublishRequest.min_importance_score` | `Score0To100` | （非持久化）| 校验由 newtype 保证 |
| `PublishCandidate.importance_score` | `Option<Score0To100>` | （非持久化）| 与 `article_ai_result_id` 同 None / 同 Some |
| `FrozenPublishItem.frozen_score` | `Option<Score0To100>` | `publish_items.frozen_score` | 见 [storage-schema §4.6](./storage-schema.md#46-publish_items) |

落地规则：在 DTO 文本中出现 `score: u8` 字面量的位置，实际类型一律为 `Score0To100`；`Option<u8>` 实际为 `Option<Score0To100>`。这一替换由 `domain` crate 在编译期保证。

## 7. 枚举类型汇总

以下枚举定义在 `domain` crate，被多个 DTO 和领域对象引用：

```text
FeedKind           = Rss | Atom | JsonFeed | RssHub
DedupDecision      = Fresh | UidDup | LinkDup | HashDup
ExtractorStrategy  = Readability | Rule | SummaryFallback
ContentQuality     = High | Medium | Fallback
ArtifactKind       = FeedPayload | HtmlPayload | AiRawResponse
BackfillTarget     = Extract | Ai

# 状态枚举（定义在 domain::state，不是 DTO）
FeedEntryState     = Discovered | DedupSkipped | PendingFetch | Fetching
                   | Extracting | Persisted | FallbackPersisted | Failed
ArticleState       = Persisted | AiPending | AiDone | ReadyForPublish
                   | PublishSkipped | Published | Retired
AiResultState      = Pending | Running | Succeeded | PermanentFailed | Filtered
                   # retryable 失败不是独立状态，失败后直接回 Pending（见 state-machine §4.2）
PublishState       = Pending | SnapshotFrozen | Rendered | StoredLocal
                   | PublishedLocal | PublishedRemote | Failed
                   # PublishedLocal 与 PublishedRemote 均为成功终态，分别对应本地模式与远端模式
                   # （见 state-machine §5 publish_records 表）
```

## 8. 命名约定：PascalCase 与 snake_case

枚举与 DTO 涉及"内存形态"和"持久化/序列化形态"两套表示，必须严格区分：

| 形态 | 命名风格 | 示例 |
|---|---|---|
| Rust 类型与变体 | PascalCase | `enum DedupDecision { Fresh, UidDup, LinkDup, HashDup }` |
| 数据库列值 | snake_case | `feed_entries.dedup_decision = 'fresh'`（或 `'hash_dup'`）|
| JSON 序列化字段值（runtime 内部 / CLI 输出）| snake_case | `{"dedup_decision": "uid_dup"}` |
| 配置文件字段值 | snake_case | `retention_policy = "on_failure"` |
| run_events.event_kind | snake_case | `entry_permanent_failed` |
| ULID / hash 字符串 | 原始大小写 | `01HZX...` / `a1b2c3...` |

**DTO 枚举可取值集合 ≠ 持久化列可取值集合**。典型例子：

- `DedupDecision` 在 runtime / JSON 中可取 4 个变体（`Fresh`/`UidDup`/`LinkDup`/`HashDup`）
- `feed_entries.dedup_decision` 列只存 `fresh` / `hash_dup` 两个（前两层去重不产生新行，见 [storage-schema §4.2](./storage-schema.md) 与 [state-machine §3.2](./state-machine.md)）
- DTO → DB 落盘时由 `storage` crate 的转换层负责过滤；DB 读回后用 DTO 枚举承载的是列上实际存在的子集

类似的"更大 DTO 枚举、更小 DB 持久化子集"关系将来可能出现在其它场景（例如被 lease reclaim 回收的 `Running` 行在下轮 claim 前会落回 `Pending`，形成"短暂可见、不稳定"的持久值），实现时由对应 repository 负责"DB 列实际可取值集合"的白名单校验。

落地规则：

- 所有定义在 `domain` crate 的枚举必须 `#[derive(serde::Serialize, serde::Deserialize)]` 并标注 `#[serde(rename_all = "snake_case")]`
- `sqlx` 派生使用 `#[sqlx(rename_all = "snake_case")]`，存为 `TEXT` 列
- 跨语言协议（CLI JSON 输出、artifact 元数据）一律 snake_case
- 文档（包括本系列文档）描述变体时使用 PascalCase（指代 Rust 类型）；描述持久化值时使用 snake_case 加反引号（指代字段值）；二者通过位置上下文区分

为何不允许 kebab-case：CLI 子命令名（如 `ai-run`、`rebuild-report`、`validate-config`）使用 kebab-case，但这是命令树层面的命名，与枚举值序列化无关。枚举值若使用 kebab-case 会与配置 TOML 的键名风格冲突。

## 9. DTO 版本责任

DTO 结构变更是 crate 间协议变更。变更规则：

- 新增可选字段（`Option<T>`）：不破坏兼容性，无需版本升级
- 修改必填字段类型或删除字段：破坏性变更，相关 crate 必须同步修改
- AI 输出结构变更：必须同步 bump `output_schema_version`（见 [storage-schema §4.4](./storage-schema.md)）
- 发布 snapshot 字段变更：必须 bump `render_version`

## 10. 与宪法的对齐检查

- §3.4 单一真相源：DTO 不是真相源，只是传输载体 ✓
- §5.4 版本责任：AI/发布相关 DTO 变更关联版本号 ✓
- 不可变性：所有 DTO 创建后不修改 ✓
- `domain` 不依赖 I/O crate：DTO 定义在 `domain`，无 I/O 依赖 ✓
