# 配置 Schema 设计

## 1. 定位

本文档是 Rust 版配置层的实现级契约。它定义配置的分层结构、每一层的字段、类型、校验规则与版本责任。

与之配套：
- 配置归属的 crate 职责见 [工程蓝图 §6.4](../plan/full-rust-rss-ai-news-blueprint.md)
- 版本记录见 [storage-schema §4.8 rule_versions](./storage-schema.md)

## 2. 配置分层

### 2.1 三个文件

| 文件 | 用途 | 格式 | 真相源位置 |
|---|---|---|---|
| `.env` | 密钥与环境敏感值 | `KEY=VALUE` | 文件系统，不进 git |
| `app.toml` | 全局应用配置 | TOML | 文件系统，进 git |
| `categories/*.toml` | 分类与订阅源配置 | TOML | 文件系统，进 git |

### 2.2 加载顺序

1. `.env` 最先加载（`dotenvy`），填入 `std::env`
2. `app.toml` 从文件系统反序列化
3. `categories/*.toml` 逐文件反序列化，合并为 `Vec<CategoryConfig>`
4. 全部加载完成后执行一次整体校验
5. 校验失败 → 打印诊断信息 → 进程退出（exit code 78 = EX_CONFIG）

### 2.3 覆盖规则

- `.env` 中的值优先于 `app.toml` 中的默认值
- 命令行 `--flag` 优先于文件配置
- 无隐式 fallback：如果某个必填字段缺失，立即报错退出，不静默使用默认值

## 3. `.env` 字段

```toml
# === 按需必填（ai.enabled=true 时需要）===
OPENAI_API_KEY = "sk-..."                # AI 调用密钥
OPENAI_BASE_URL = "https://api.openai.com/v1"  # AI endpoint

# === 按需必填（publish 命令且非本地模式时需要）===
GITHUB_TOKEN = "ghp_..."                 # GitHub 发布令牌

# === 可选 ===
RSSHUB_BASE_URL = "https://rsshub.example.com"  # RSSHub 实例地址
HTTP_PROXY = ""                          # HTTP 代理
HTTPS_PROXY = ""                         # HTTPS 代理
DATABASE_URL = ""                        # 覆盖默认数据库路径（PG 场景必填）
```

校验规则（条件式校验，不存在"无条件必填"）：

- `OPENAI_API_KEY`：当 `config.ai.enabled=true` 时必须非空字符串；当 `ai.enabled=false` 时允许缺省或空字符串，且 `ai` crate 不构造 OpenAI client
- `OPENAI_BASE_URL`：当 `config.ai.enabled=true` 时必须是合法 URL；`ai.enabled=false` 时不校验
- `GITHUB_TOKEN`：仅在 `publish` 命令执行**且**未指定 `--local-only` / 配置 `publish.github_owner` 非空时校验非空
- `RSSHUB_BASE_URL`：若任一 category 的 source 使用了 `{RSSHUB}` 占位符，则必须非空

## 4. `app.toml` Schema

```toml
# === 配置版本 ===
schema_version = "1"

# === 数据库 ===
[database]
driver = "sqlite"                        # "sqlite" | "postgres"
sqlite_path = "data/rss-ai-news.db"      # 相对于工作目录
max_connections = 5
busy_timeout_ms = 5000

# === 网络 ===
[http]
user_agent = "RSS-AI-News/1.0"
timeout_seconds = 30
max_retries = 3
retry_backoff_base_ms = 1000
concurrent_feeds = 10
concurrent_fetches = 5

# === AI ===
[ai]
enabled = true                           # false 时走无 AI 发布降级（见 state-machine §4.1.3）
model = "gpt-4o-mini"
max_tokens = 4096
temperature = 0.3
request_timeout_seconds = 60
max_input_chars = 8000                   # 输入截断阈值

[ai.rate_limit]                          # governor 配置
requests_per_minute = 60                 # 必须 > 0；映射为 governor::Quota::per_minute(..)
tokens_per_minute = 0                    # 0 = 禁用 TPM 限制（ai crate 跳过 Quota 构造，不传 0 给 governor）；>0 时以 Quota 表达 TPM

# === 发布 ===
[publish]
target_timezone = "Asia/Shanghai"
github_owner = ""                        # 空 → 本地发布模式（published_local 终态）；非空 → 远端模式
github_repo = ""
github_branch = "main"
github_path_prefix = "archive"
local_output_dir = "output"
include_unscored = false                 # 直通发布开关；语义见下方 ai.enabled × include_unscored 真值表

# === 去重 ===
[dedup]
enable_link_dedup = true
enable_content_dedup = true
link_normalizer_version = "1"

# === 正文提取 ===
[extractor]
strategy_order = ["readability", "summary_fallback"]
max_body_bytes = 1_048_576               # 1 MB
min_body_chars = 100                     # 低于此视为 fallback

# === 租约 ===
[lease]
fetch_duration_seconds = 300
ai_duration_seconds = 600
publish_duration_seconds = 600
reclaim_interval_seconds = 120

# === 重试 ===
[retry]
feed_entry_max_attempts = 5
ai_max_attempts = 3
publish_max_attempts = 5

# === Raw Artifact ===
[artifact]
retention_policy = "on_failure"          # "always" | "on_failure" | "sampled" | "debug_only" | "off"
sample_rate = 0.1                        # retention_policy = "sampled" 时生效
inline_threshold_bytes = 65536           # 小于此阈值存 inline，大于存文件
file_storage_dir = "data/artifacts"
ttl_days = 30                            # expires_at 计算依据

# === 观测 ===
[observability]
log_level = "info"                       # "trace" | "debug" | "info" | "warn" | "error"
log_format = "pretty"                    # "pretty" | "json"
log_file = ""                            # 空则仅 stdout
enable_metrics = false
metrics_bind = "127.0.0.1:9090"
```

### 4.1 `ai.enabled × publish.include_unscored` 真值表

本表中的 `include_unscored` 指 **effective config**：`category.publish_override.include_unscored` 存在时覆盖全局 `publish.include_unscored`，否则继承全局值（见 [§5](#5-categoriestoml-schema)）。`ai.enabled` 仅来自全局 `[ai]`，**不**支持 category 级覆盖（避免 effective 状态空间膨胀；分类内 AI 行为差异通过 `[category.ai_override]` 的 prompt / model / max_input_chars 表达）。`include_unscored` 控制"未经 AI 评分的 article 是否进入发布候选"，与 `ai.enabled` 组合产生 4 种行为：

| `ai.enabled` | `include_unscored` | 候选源 | `publish_items.article_ai_result_id` | 发布行为 |
|---|---|---|---|---|
| `true` | `false` | 仅 `articles.state='ready_for_publish'` 且有 `article_ai_results.state='succeeded' AND keep_decision=1` | `NOT NULL` | 走 AI 路径（I4.a / I4'.a）|
| `true` | `true` | 同 `ai=true/include=false` | `NOT NULL` | 同上；`include_unscored` 在 `ai.enabled=true` 时无效（详见表后说明） |
| `false` | `false` | 空 | 不适用 | 没有可发布候选，publish 退出码 0、产生 0 条 publish_items |
| `false` | `true` | `articles.state='persisted'` 或 `'ready_for_publish'` 且 [无任何 AI 行](./state-machine.md#413-ai-关闭--无-ai-发布降级)；`persisted` 候选在入选 freeze 事务内升格为 `ready_for_publish` | `NULL` | 走直通路径（I4.b / I4'.b）；`frozen_summary` 取 `articles.summary_raw`，`frozen_tags_json='[]'`，`frozen_score=NULL` |

**`include_unscored` 不是 AI failure fallback**：当 `ai.enabled=true` 时，即使 `include_unscored=true` 也不会让 `permanent_failed` / `filtered` 的 article 绕过 AI 直接发布。AI 永久失败的 article 必须经 `backfill --target ai`（新模型 / 修正 prompt）重跑后才能进入 `ready_for_publish`。完整流程见 [state-machine §4.1.3](./state-machine.md#413-ai-关闭--无-ai-发布降级)。

## 5. `categories/*.toml` Schema

每个文件代表一个分类：

```toml
# categories/ai.toml
schema_version = "1"

[category]
key = "ai"
display_name = "AI & Machine Learning"
priority = 10                            # 越小越优先

# === AI 定制（覆盖 app.toml 的全局 AI 设置）===
[category.ai_override]
prompt_template = """
你是一个 AI/ML 领域的新闻分析师。请分析以下文章...
"""
max_input_chars = 10000                  # 覆盖全局
model = ""                               # 空则沿用全局

# === 发布定制 ===
[category.publish_override]
max_items_per_report = 30
min_importance_score = 30
include_unscored = false                 # AI 关闭时是否仍发布

# === 订阅源列表 ===
[[sources]]
key = "openai-blog"
display_name = "OpenAI Blog"
feed_url = "https://openai.com/blog/rss.xml"
feed_kind = "rss"                        # "rss" | "atom" | "json_feed" | "rsshub"
priority = 10
enabled = true

[[sources]]
key = "huggingface-papers"
display_name = "HuggingFace Daily Papers"
feed_url = "{RSSHUB}/huggingface/daily-papers"
feed_kind = "rsshub"
priority = 20
enabled = true
```

## 6. 校验规则

### 6.1 启动时整体校验

| 校验项 | 条件 | 失败行为 |
|---|---|---|
| `schema_version` 存在且受支持 | 所有 TOML 文件 | exit 78 |
| `category.key` 全局唯一 | 跨文件 | exit 78 |
| `sources[].key` 分类内唯一 | 同一文件内 | exit 78 |
| `sources[].feed_url` 非空合法 URL | 每条 source | exit 78 |
| `{RSSHUB}` 占位符时 `RSSHUB_BASE_URL` 非空 | 关联检查 | exit 78 |
| `publish.target_timezone` 是合法 IANA tz | app.toml | exit 78 |
| `database.driver` 取值合法 | app.toml | exit 78 |
| 数值范围合理（timeout > 0, retries >= 0 等）| 全局 | exit 78 |

### 6.2 命令特定校验

某些字段只在特定命令执行时才需要，且全部为**条件式校验**（与 §3 一致）：

- `publish`（远端模式）：当未指定 `--local-only` 且 `publish.github_owner` 非空时，校验 `GITHUB_TOKEN` / `publish.github_repo` 非空；本地模式下三者均不校验
- `ai-run`：当 effective `config.ai.enabled=true` 时校验 `OPENAI_API_KEY` 非空、`OPENAI_BASE_URL` 合法 URL；当 `ai.enabled=false` 时返回配置语义错误（exit 78），提示 `ai-run` 与 AI 关闭模式互斥
- `doctor`：执行 §6.1 通用校验 + §3 条件式 env 校验 + 上述命令特定校验的全集；不存在"无条件校验所有字段"

### 6.3 校验报告格式

校验失败时必须输出结构化诊断：

```text
Configuration error:
  [app.toml] database.driver: expected one of "sqlite", "postgres", got "mysql"
  [categories/ai.toml] sources[1].feed_url: invalid URL "{RSSHUB}/invalid url"
  [.env] RSSHUB_BASE_URL: required because categories/ai.toml uses {RSSHUB} placeholder
```

## 7. 版本责任

### 7.1 `schema_version`

- `app.toml` 和每个 `categories/*.toml` 都必须声明 `schema_version`
- `config` crate 启动时检查版本兼容性
- 不兼容的 schema_version → exit 78 + 提示用户升级

### 7.2 与 `rule_versions` 的关联

配置加载成功后，`config` crate 计算配置内容的 SHA256，与 `rule_versions` 表中 `kind='config'` 的最新行对比：

- 相同 → 复用现有 `rule_versions.id`
- 不同 → 插入新行，`version_tag` 自动递增

这保证了每条 `feed_sources.config_version` 都能追溯到产生它的配置快照。

## 8. Rust 类型映射

```text
AppConfig
├── schema_version: String
├── database: DatabaseConfig
│   ├── driver: DatabaseDriver          # enum { Sqlite, Postgres }
│   ├── sqlite_path: PathBuf
│   ├── max_connections: u32
│   └── busy_timeout_ms: u64
├── http: HttpConfig
├── ai: AiConfig
├── publish: PublishConfig
├── dedup: DedupConfig
├── extractor: ExtractorConfig
├── lease: LeaseConfig
├── retry: RetryConfig
├── artifact: ArtifactConfig
└── observability: ObservabilityConfig

CategoryConfig
├── schema_version: String
├── category: CategoryMeta
│   ├── key: String
│   ├── display_name: String
│   └── priority: u32
├── ai_override: Option<AiOverride>
├── publish_override: Option<PublishOverride>
└── sources: Vec<SourceConfig>

SourceConfig
├── key: String
├── display_name: String
├── feed_url: String                    # 含 {RSSHUB} 占位符
├── feed_kind: FeedKind                 # enum { Rss, Atom, JsonFeed, RssHub }
├── priority: u32
└── enabled: bool
```

所有 Config 结构体使用 `#[derive(Clone, Debug, serde::Deserialize)]`。`config` crate 内部持有 `Arc<AppConfig>` 和 `Arc<Vec<CategoryConfig>>`，运行时不可变。

## 9. CLI 覆盖点

以下 `app.toml` 字段可被 CLI flag 覆盖：

| CLI flag | 覆盖目标 |
|---|---|
| `--db-path <path>` | `database.sqlite_path` |
| `--log-level <level>` | `observability.log_level` |
| `--log-format <fmt>` | `observability.log_format` |
| `--dry-run` | 全局 flag，禁止写入操作 |
| `--category <key>` | 过滤只处理指定分类 |
| `--timezone <tz>` | `publish.target_timezone` |

覆盖在 `app.toml` 反序列化之后、校验之前应用。

## 10. 与宪法的对齐检查

- §3.4 单一真相源：配置真相源为文件系统结构化配置 + `rule_versions` 版本追踪 ✓
- §5.4 版本责任：`schema_version` + `rule_versions` SHA256 关联 ✓
- §5.1 失败路径：校验失败 → exit 78 + 结构化诊断 ✓
- §6.2 退出路径：配置变更只产生新 version，旧 version 永久保留 ✓
