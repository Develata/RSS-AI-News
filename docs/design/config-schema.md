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
RSSHUB_ACCESS_KEY = ""                          # RSSHub ACCESS_KEY，设置后自动追加到 rsshub 源
HTTP_PROXY = ""                          # HTTP 代理
HTTPS_PROXY = ""                         # HTTPS 代理
DATABASE_URL = ""                        # 覆盖默认数据库路径（PG 场景必填）
```

校验规则（条件式校验，不存在"无条件必填"）：

- `OPENAI_API_KEY`：当 `config.ai.enabled=true` 时必须非空字符串；当 `ai.enabled=false` 时允许缺省或空字符串，且 `ai` crate 不构造 OpenAI client
- `OPENAI_BASE_URL`：当 `config.ai.enabled=true` 时必须是合法 URL；`ai.enabled=false` 时不校验
- `GITHUB_TOKEN`：仅在 `publish` 命令执行**且**未指定 `--local-only` / 配置 `publish.github_owner` 非空时校验非空
- `RSSHUB_BASE_URL`：若任一 category 的 source 使用了 `{RSSHUB}` 占位符，则必须非空
- `RSSHUB_ACCESS_KEY`：可选；当 source 的 `feed_kind="rsshub"` 且 URL 未显式包含 `key=` 时，加载阶段自动追加为 `?key=...` 或 `&key=...`

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
max_items_per_report = 30                # 单次报告最大入选数；映射 NonZeroU32（必须 ≥ 1）；可被 [category.publish_override] 按字段覆盖（见 §4.5）
min_importance_score = 30                # 0-100，AI 路径下的发布门槛；0 表示"显式无下限"，与缺省语义不同；可被 [category.publish_override] 按字段覆盖；ai.enabled=false 直通路径下不参与过滤
candidate_window_hours = 48              # 发布候选时间窗口；按 COALESCE(feed.published_at, feed.discovered_at) 过滤 [now-Nh, now]；0 = 不限制下界，仍排除未来时间
include_unscored = false                 # 直通发布开关；语义见下方 ai.enabled × include_unscored 真值表；可被 [category.publish_override] 按字段覆盖

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

# === 单次 run 工作量边界 ===
[runtime]
max_batches_per_run = 10                 # extract claim 循环 / ai-run process 阶段 claim 循环上限；0 = 不限（仅由 lease/宿主超时兜底）

# === Raw Artifact ===
# v0.1.0：仅 inline 路径生效；inline_threshold_bytes / file_storage_dir
# 为 v0.2 file-backed 路径预留（参见 replay-and-artifacts §2.3）。
[artifact]
retention_policy = "on_failure"          # 默认；"always" | "on_failure" | "sampled" | "debug_only" | "off"。on_failure = 解析前总是捕获并独立事务 commit，关联操作成功后同步清理；详见 replay-and-artifacts §3.1 / §3.2
sample_rate = 0.1                        # retention_policy = "sampled" 时生效
inline_threshold_bytes = 65536           # v0.2：小于此阈值存 inline，大于存文件；v0.1.0 全 inline 不消费
file_storage_dir = "data/artifacts"      # v0.2：file-backed artifact 落盘根目录；v0.1.0 不消费
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
| `false` | `true` | `articles.state='persisted'` 或 `'ready_for_publish'` 且 [无任何 AI 行](./state-machine.md#413-ai-关闭--无-ai-发布降级)；`persisted` 候选在入选 freeze 事务内升格为 `ready_for_publish` | `NULL` | 走直通路径（I4.b / I4'.b）；`frozen_summary` 取 `feed_entries.summary_raw`（`articles.origin_feed_entry_id` 关联），`frozen_tags_json='[]'`，`frozen_score=NULL` |

**`include_unscored` 不是 AI failure fallback**：当 `ai.enabled=true` 时，即使 `include_unscored=true` 也不会让 `permanent_failed` / `filtered` 的 article 绕过 AI 直接发布。AI 永久失败的 article 必须经 `backfill --target ai`（新模型 / 修正 prompt）重跑后才能进入 `ready_for_publish`。完整流程见 [state-machine §4.1.3](./state-machine.md#413-ai-关闭--无-ai-发布降级)。

### 4.2 `[http]` 字段语义

`[http]` 段同时影响 ingest（feed 拉取）、extract（详情页抓取）与 doctor 探活，下表给出每个字段的作用域、默认值与默认值理由。

| 字段 | 作用阶段 | 作用域 | 默认值 | 说明 |
|---|---|---|---|---|
| `user_agent` | 全部 HTTP 请求 | 单进程 | `"RSS-AI-News/1.0"` | 同一进程内所有 HTTP 客户端共享；不允许 per-source 覆盖 |
| `timeout_seconds` | 全部 HTTP 请求 | 单请求 | `30` | 端到端超时；不区分 connect / read |
| `max_retries` / `retry_backoff_base_ms` | 全部 HTTP 请求 | 单请求 | `3` / `1000` | reqwest 客户端层指数退避；与状态机 retry budget（§4 `[retry]`）不同维度，前者覆盖瞬时网络故障，后者控制业务级重试 |
| `concurrent_feeds` | ingest（`runtime::ingest`）| 全局并发预算 | `10` | feed 源拉取阶段的最大并发请求数（`futures::stream::buffer_unordered(concurrent_feeds)`）；与 CLI `--batch-size` 是不同维度——`--batch-size` 控制单次 run 处理的 feed 行数上限，`concurrent_feeds` 控制其中并发执行的请求数。`min(batch_size, concurrent_feeds)` 决定实际峰值并发 |
| `concurrent_fetches` | extract（`runtime::extract`）| 全局并发预算 | `5` | 详情页 HTML 抓取阶段的最大并发请求数；与 `concurrent_feeds` 独立计数，二者可同时跑满。CLI `--batch-size` 同样仅控制本轮处理的行数上限 |

并发预算在 runtime 启动时构造一次 `Semaphore(concurrent_feeds)` / `Semaphore(concurrent_fetches)` 并向各阶段共享；不允许 per-source / per-category 覆盖（避免 effective 配置空间膨胀）。如需限速到 source 粒度，使用 `governor` 在 ingest crate 内追加 quota，而非通过本段配置。

### 4.3 HTTP 代理传播

`HTTP_PROXY` / `HTTPS_PROXY` 通过 `.env` 注入，不进入 `app.toml`。其传播路径如下：

1. `config::env::EnvConfig` 在加载阶段读取 `HTTP_PROXY` / `HTTPS_PROXY`（也兼容大写变体），存为 `Option<String>`，并对非空值校验是否为合法 URL；非法 → 启动失败 exit 78
2. `runtime` 构造共享 `reqwest::Client` 时显式调用 `reqwest::Proxy::http(url)` / `reqwest::Proxy::https(url)`；**不依赖** reqwest 默认的进程环境读取，避免子进程或被覆盖环境变量造成行为漂移
3. 同一 client 被 ingest / extract / ai / doctor 共用；不存在 per-command 代理覆盖。如需临时绕过代理，请通过 `unset HTTP_PROXY HTTPS_PROXY` 后重启 runtime
4. PG 连接、SQLite 等非 HTTP 调用不受本配置影响

**校验**：当 `HTTP_PROXY` / `HTTPS_PROXY` 设置但格式非法时，`validate-config` 与 `doctor` 都返回 FAIL；`doctor preflight` 在网络可达性检查时按代理路径访问。

### 4.4 `[runtime]` 字段语义

`[runtime]` 段控制单次 CLI 运行内部的工作量边界，与跨运行调度无关（跨运行调度由宿主 cron / GitHub Actions 负责，见 [蓝图 §14.3](../plan/full-rust-rss-ai-news-blueprint.md)）。

| 字段 | 作用阶段 | 单位 | 默认值 | 说明 |
|---|---|---|---|---|
| `max_batches_per_run` | extract claim 循环 / ai-run process 阶段 claim 循环 | 批次数 | `10` | 单次 run 内部 claim 循环最多处理多少批；与 `--batch-size` 相乘得到本次运行的处理上限（如 `batch_size=50 × max_batches_per_run=10` = 500 条/run）。`0` 表示不限，仅由 lease 过期 + 宿主超时兜底。CLI `--max-batches` 可覆盖 |

**作用域精确边界**（F8-2 W4-2）：`max_batches_per_run` 仅约束以下两个 claim 循环：

- **extract claim 循环**：`ExtractFlow::run` 对 `feed_entries.state='pending_fetch'` 行的 claim → 处理 → release 循环（一批 = 一次 `claim_pending_fetch` 调用）
- **ai-run process 阶段 claim 循环**：`AiRunFlow::process_ai_tasks` 对 `article_ai_results.state='pending'` 行的 claim → 调 AI → release 循环

**不在作用域内的阶段**：

- **ingest 内部 fetch 阶段**：`IngestFlow` 的 source 遍历（`FeedFlow::run`）由 active sources 列表天然约束（典型 < 100 源），不属于"分批 claim 循环"语义；规模超过千级时由 `[http].concurrent_feeds` + 宿主超时兜底
- **ai-run task_gen 阶段**：one-shot `list_persisted_for_ai_task_gen` 扫描后插入 pending 任务（不是 claim 循环），仅受 `--batch-size` 限制扫描页大小
- **publish 全部阶段**：见下文"与 `publish` 命令的关系"

**触达上限的退出语义**：达到 `max_batches_per_run` 后，runtime 退出本阶段循环并返回 exit code 0（视为本次配额完成，不是错误），同时写一行 INFO 日志：

```text
[INFO] max_batches_per_run reached (10 batches × 50 size = 500 items processed); leaving N pending entries for next run
```

剩余 `pending` 条目自然由下一次 cron / 宿主调度的 run 继续处理，符合"宿主负责调度，进程负责单次执行"的分工。

**与 `[lease]` / 宿主超时的关系**：三者构成单次 run 的边界三层兜底：

1. `max_batches_per_run`（应用层显式上限，主防线）
2. `lease.*_duration_seconds` 过期回收（防止单批次卡死，副防线）
3. 宿主超时（GitHub Actions 360 min / 自定义 cron 超时，最后兜底）

backfill 等已知大量待处理场景，调用方应显式 `--max-batches=0` 解除应用层上限，仅依赖 lease 与宿主超时；常规增量场景维持默认值 `10`。

**与 `publish` 命令的关系**：`publish` 不受 `max_batches_per_run` 控制（publish 的工作量天然受当日 `ready_for_publish` 候选集与 `max_items_per_report` 限制；不存在"分多批跨 run"的概念）。

### 4.5 effective publish 配置（全局默认 + category 覆盖）

`[publish]` 全局段与 `[category.publish_override]` 之间采用**按字段覆盖**（不是"整个 override 表覆盖"）。effective 计算规则：

```text
effective_publish.max_items_per_report =
    category.publish_override.max_items_per_report.unwrap_or(publish.max_items_per_report)

effective_publish.min_importance_score =
    category.publish_override.min_importance_score.unwrap_or(publish.min_importance_score)

effective_publish.include_unscored =
    category.publish_override.include_unscored.unwrap_or(publish.include_unscored)

effective_publish.path_template =
    category.publish_override.path_template.unwrap_or(publish.template.path_template)
```

关键约束：

- **零值与 false 是显式覆盖，不是缺省**：`min_importance_score = 0` 表示"显式无下限"，`include_unscored = false` 表示"显式禁用直通"。缺省只能由 TOML 中**字段缺席**表达；`config` crate 反序列化时把缺席映射为 `None`、把 `0` / `false` 映射为 `Some(0)` / `Some(false)`。详见 §8 的 Rust 类型签名
- **`max_items_per_report` 必须 ≥ 1**：用 `NonZeroU32` 表达；不允许 `0`（既不解释为"无限制"也不解释为"空报告"）
- **AI 关闭时各字段语义**：
    - `min_importance_score`：直通路径无 AI score，**不参与过滤**（即使分类显式覆盖也无效）
    - `max_items_per_report`：**仍生效**（限制报告规模与 AI 路径独立）
    - `include_unscored`：见 §4.1 真值表；只决定 `persisted` 是否进入候选
- **路径模板覆盖**：`path_template` 控制本地输出与远端相对路径。全局模板必须包含分类占位符和日期占位符；分类级覆盖可以省略分类占位符，但仍必须包含日期占位符，且调用方必须保证不同分类不会写到同一路径。

`runtime::publish::freeze` 在构造 [`PublishRequest`](./internal-dto-contracts.md#51-publishrequest) 前完成上述合并，把 effective 值写入 DTO；DTO 的 `max_items` / `min_importance_score` 是必填非 Option，因此**必须**在 freeze 阶段消解 `None`。

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

# === 发布定制（按字段覆盖 [publish] 全局默认；任意字段省略即继承全局；详见 §4.5）===
[category.publish_override]
max_items_per_report = 30                # NonZeroU32；省略 → 继承 publish.max_items_per_report
min_importance_score = 30                # 0-100；显式 0 = 无下限（≠ 缺省）；省略 → 继承 publish.min_importance_score；ai.enabled=false 直通路径下不参与过滤
include_unscored = false                 # 显式 false ≠ 缺省；省略 → 继承 publish.include_unscored；AI 关闭时是否仍发布
path_template = "ai/{YYYY}/{YYYYMMDD}.md" # 可选；省略 → 继承 publish.template.path_template；必须是相对路径并包含日期占位符

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
│   ├── target_timezone: String
│   ├── github_owner: String              # 空 → 本地发布模式
│   ├── github_repo: String
│   ├── github_branch: String
│   ├── github_path_prefix: String
│   ├── local_output_dir: PathBuf
│   ├── max_items_per_report: NonZeroU32  # ≥ 1；effective 来源见 §4.5
│   ├── min_importance_score: Score0To100 # 0-100；effective 来源见 §4.5
│   └── include_unscored: bool            # effective 来源见 §4.5
├── dedup: DedupConfig
├── extractor: ExtractorConfig
├── lease: LeaseConfig
├── retry: RetryConfig
├── runtime: RuntimeConfig
│   └── max_batches_per_run: u32         # 0 = 不限
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
│   ├── max_items_per_report: Option<NonZeroU32>  # None = 继承全局；Some(n) = 显式覆盖
│   ├── min_importance_score: Option<Score0To100> # None = 继承全局；Some(0) = 显式无下限
│   ├── include_unscored: Option<bool>            # None = 继承全局；Some(false) = 显式禁用
│   └── path_template: Option<String>             # None = 继承 publish.template.path_template
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
| `--max-batches <n>` | `runtime.max_batches_per_run`（仅 `ingest` / `ai-run` / `run`）；`0` = 不限 |

覆盖在 `app.toml` 反序列化之后、校验之前应用。

## 10. 与宪法的对齐检查

- §3.4 单一真相源：配置真相源为文件系统结构化配置 + `rule_versions` 版本追踪 ✓
- §5.4 版本责任：`schema_version` + `rule_versions` SHA256 关联 ✓
- §5.1 失败路径：校验失败 → exit 78 + 结构化诊断 ✓
- §6.2 退出路径：配置变更只产生新 version，旧 version 永久保留 ✓
