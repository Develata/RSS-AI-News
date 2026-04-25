# 错误模型与可观测性设计

## 1. 定位

本文档定义 Rust 版的错误分类体系、传播规则、观测点和验证方式。它是宪法 §5（失败→观测→验证内建）的实现级契约。

与之配套：
- 存储层错误见 [storage-schema §8](./storage-schema.md)
- 状态机失败分支见 [state-machine](./state-machine.md)
- 持久化事件表见 [storage-schema §4.9 run_events](./storage-schema.md)

## 2. 错误分类体系

### 2.1 三层错误枚举

```text
层一：能力执行层错误（各 crate 内部）
├── FeedError        — feed crate
├── ExtractorError   — extractor crate
├── AiError          — ai crate
├── StorageError     — storage crate
├── PublishError     — publish crate
└── ReportError      — report crate

层二：流程协调层错误（runtime crate）
└── RuntimeError
    ├── Feed(FeedError)
    ├── Extractor(ExtractorError)
    ├── Ai(AiError)
    ├── Storage(StorageError)
    ├── Publish(PublishError)
    ├── Report(ReportError)
    ├── LeaseConflict { table, id, expected_owner }
    ├── RetryBudgetExhausted { table, id, attempts }
    └── Cancelled

层三：交互层错误（cli / app crate）
└── AppError
    ├── Config(ConfigError)
    ├── Runtime(RuntimeError)
    └── Io(std::io::Error)
```

### 2.2 每个底层错误必须携带的元信息

```text
trait ClassifiedError {
    fn is_retryable(&self) -> bool;
    fn error_kind(&self) -> &str;       // 存入 last_error_kind 列
    fn display_user(&self) -> String;   // CLI 用户可读
    fn display_debug(&self) -> String;  // 日志/诊断详情
}
```

### 2.3 各 crate 错误枚举

#### `FeedError`

| 变体 | retryable | error_kind | 典型原因 |
|---|---|---|---|
| `HttpTimeout` | true | `http_timeout` | 超时 |
| `HttpStatus { code }` | 5xx true, 4xx false | `http_4xx` / `http_5xx` | 服务端/客户端错误 |
| `ConnectionFailed` | true | `connection_failed` | DNS / TLS / 连接拒绝 |
| `ParseFailed { reason }` | false | `feed_parse` | feed XML/JSON 格式错误 |
| `TooLarge { bytes }` | false | `too_large` | 响应超过限额 |
| `InvalidUrl` | false | `invalid_url` | URL 格式错误 |

#### `ExtractorError`

| 变体 | retryable | error_kind |
|---|---|---|
| `HttpTimeout` | true | `http_timeout` |
| `HttpStatus { code }` | 同上 | `http_4xx` / `http_5xx` |
| `ConnectionFailed` | true | `connection_failed` |
| `TooLarge { bytes }` | false | `too_large` |
| `ParseFailed` | false | `html_parse` |
| `ContentTooShort { chars }` | false | `content_too_short` |

#### `AiError`

| 变体 | retryable | error_kind |
|---|---|---|
| `RateLimited { retry_after }` | true | `rate_limited` |
| `HttpTimeout` | true | `http_timeout` |
| `HttpStatus { code }` | 5xx true, 4xx false | `http_4xx` / `http_5xx` |
| `ConnectionFailed` | true | `connection_failed` |
| `InvalidResponse { reason }` | false | `invalid_response` |
| `OutputParseFailed { raw }` | false | `output_parse` |
| `ContentFiltered` | false | `content_filtered` |
| `ModelNotFound` | false | `model_not_found` |

#### `StorageError`

| 变体 | retryable | error_kind |
|---|---|---|
| `Conflict { table, key }` | false | `conflict` |
| `Integrity { table, reference }` | false | `integrity` |
| `Unavailable` | true | `db_unavailable` |
| `Timeout` | true | `db_timeout` |
| `Corruption` | false | `corruption` |
| `MigrationFailed { version }` | false | `migration` |

#### `PublishError`

| 变体 | retryable | error_kind |
|---|---|---|
| `GitHubApiError { status, message }` | 5xx true, 4xx false | `github_api` |
| `GitHubRateLimit { reset_at }` | true | `github_rate_limit` |
| `GitHubAuthFailed` | false | `github_auth` |
| `LocalIoError` | 运行时按 `io::ErrorKind` 判定（`WouldBlock`/`Interrupted`/`TimedOut` → true，其余 false）| `local_io` |
| `SnapshotEmpty` | false | `snapshot_empty` |

#### `ConfigError`

| 变体 | retryable | error_kind |
|---|---|---|
| `FileNotFound { path }` | false | `config_not_found` |
| `ParseFailed { path, reason }` | false | `config_parse` |
| `ValidationFailed { errors }` | false | `config_validation` |
| `VersionMismatch { expected, got }` | false | `config_version` |

## 3. 错误传播规则

### 3.1 能力层 → 流程协调层

- 能力层错误通过 `RuntimeError::*` 包装向上传递
- `runtime` 根据 `is_retryable()` 决定：
  - `true` → 释放 lease 为可重试（`state` 回退，`attempt_count` 已递增）
  - `false` → 释放 lease 为永久失败（`state` 进入终态）
- 无论哪种，必须同时写 `last_error` + `last_error_kind` 到对应行

### 3.2 流程协调层 → 交互层

- `RuntimeError` 通过 `AppError::Runtime` 传递到 CLI
- CLI 根据 `display_user()` 输出用户可读信息
- CLI 根据 exit code 区分错误类别：
  - `0` — 成功
  - `1` — 运行时错误（网络、AI、发布）
  - `2` — 用户输入错误（参数、命令）
  - `78` — 配置错误（EX_CONFIG）

### 3.3 绝不静默吞掉错误

- 禁止 `let _ = fallible_fn();`
- 禁止 `if let Ok(x) = ... { } // 忽略 Err`
- 允许的唯一例外：日志写入本身失败（避免递归 panic）

## 4. 可观测性架构

### 4.1 三个层次

| 层次 | 工具 | 目的 | 持久化 |
|---|---|---|---|
| 结构化日志 | `tracing` | 开发调试、运维排查 | 文件 / stdout（可配置）|
| 关键事件 | `run_events` 表 | 业务审计、故障追溯 | SQLite / PG |
| 指标 | `metrics` crate | 趋势监控、告警 | Prometheus endpoint（可选）|

### 4.2 `tracing` 设计

#### Span 层次

```text
run(run_id)
└── stage(ingest | ai_run | publish | ...)
    └── source(category_key, source_key)
        └── entry(feed_entry_uid)
            └── operation(fetch | extract | ai_call | ...)
```

#### 必须记录的 span 字段

- `run_id`：本次 CLI 执行的 ULID，全局唯一
- `stage`：当前阶段
- `category_key` / `source_key`：关联的分类和源
- `entry_id` / `article_id`：关联的对象 ID

#### 日志级别约定

| 级别 | 用途 |
|---|---|
| `ERROR` | 不可恢复的失败（永久失败、数据损坏）|
| `WARN` | 可恢复的失败（重试、降级、lease 冲突）|
| `INFO` | 业务里程碑（批次开始/结束、发布成功、配置加载）|
| `DEBUG` | 详细执行流程（单条 entry 处理步骤）|
| `TRACE` | 原始数据（HTTP 响应片段、SQL 参数）|

#### 输出格式

- `pretty`（默认）：人类可读，带颜色（tty 检测）
- `json`：结构化 JSON，每行一条事件，适合日志聚合

两种格式通过 `tracing-subscriber` 的 `Layer` 组合实现，运行时由 `config.observability.log_format` 选择。

#### 密钥与敏感字段的 redaction 策略

**禁止出现在任何日志 / `run_events` / `--output-format json` 输出中的值**：

| 类别 | 具体字段 |
|---|---|
| 环境变量 | `OPENAI_API_KEY`, `GITHUB_TOKEN`, `RSSHUB_BASE_URL` 的凭证参数部分 |
| HTTP header | `Authorization`, `X-Api-Key`, `Cookie`, `Set-Cookie`, `Proxy-Authorization` |
| 配置字段 | 任何名称以 `_token` / `_key` / `_secret` / `_password` 结尾的字段 |
| URL 成分 | userinfo 部分（`https://user:pass@host/`）|
| AI 响应 | 原始 body 不写入日志，只写 artifact（受 retention_policy 控制）|

**实现规则**：

- 所有从配置 / 环境变量读取到的密钥值统一封装在 `domain::SecretString` 新类型中
- `SecretString` 的 `Debug` / `Display` 实现固定输出 `"***"`
- `serde::Serialize` 默认跳过，必须显式调用 `expose_secret()` 才能拿到原值
- HTTP client 层有统一的 `redact_headers` 过滤器，日志中 `Authorization` 等 header 值始终替换为 `"Bearer ***"`
- 提交 `run_events.context_json` 前经过同一套过滤，基于字段名前缀与正则匹配

**测试要求**：为 redaction 写反向断言测试——故意在 `context_json` 中放入伪密钥，断言日志输出中不含原值。

**fail-close**：若 redaction 过滤器本身出错（如正则编译失败），直接跳过该条事件而非输出原值。

### 4.3 `run_events` 持久化

不是日志的替代品，而是日志流中"值得持久化到数据库"的业务事件子集。

#### 必须写入 `run_events` 的事件

| event_kind | severity | 触发时机 |
|---|---|---|
| `run_started` | info | CLI 命令执行开始 |
| `run_completed` | info | CLI 命令正常结束 |
| `run_failed` | error | CLI 命令异常结束 |
| `source_fetch_failed` | warn | feed 拉取失败 |
| `entry_dedup_skipped` | info | 条目去重跳过（批量摘要）|
| `entry_permanent_failed` | error | 条目永久失败 |
| `ai_permanent_failed` | error | AI 永久失败 |
| `ai_content_filtered` | warn | AI 内容被过滤 |
| `publish_started` | info | 发布批次开始 |
| `publish_succeeded` | info | 发布成功 |
| `publish_failed` | error | 发布失败 |
| `lease_reclaimed` | warn | 过期 lease 被回收 |
| `artifact_cleaned` | info | artifact 按 TTL 清理 |
| `config_version_changed` | info | 配置版本变更 |
| `migration_applied` | info | 数据库 migration 执行 |

#### `target_kind` / `target_id` 约束

每条事件按以下规则填充 `target_kind` 与 `target_id`：

- 取值范围由 [storage-schema §4.9](./storage-schema.md) 定义的枚举唯一决定，不在本文档重复
- 关联到具体真相源对象时，`target_kind` 与 `target_id` 必须同时非空
- 无关联对象的全局事件（如 `run_started` / `run_completed` / `run_failed` / `migration_applied` / `config_version_changed`）必须同时为 NULL
- 批量摘要事件（如 `entry_dedup_skipped` 聚合多条）`target_kind` 设为对应对象，`target_id` 留 NULL，并在 `context_json` 中提供 ID 列表

事件与对象类别的典型对应：

| event_kind | target_kind |
|---|---|
| `source_fetch_failed` | `feed_source` |
| `entry_dedup_skipped` | `feed_entry`（target_id NULL，批量）|
| `entry_permanent_failed` | `feed_entry` |
| `ai_permanent_failed` / `ai_content_filtered` | `article_ai_result` |
| `publish_started` / `publish_succeeded` / `publish_failed` | `publish_record` |
| `lease_reclaimed` | 取自被回收对象（`feed_entry` / `article_ai_result` / `publish_record`）|
| `artifact_cleaned` | `raw_artifact` |
| `run_started` / `run_completed` / `run_failed` / `migration_applied` / `config_version_changed` | NULL |

#### 写入约束

- `run_events` 写入失败不得阻塞主流程——降级为 `tracing::error!` 记录
- `context_json` 最大 4 KB，超出截断并标注 `"truncated": true`
- 批量事件（如去重跳过）可聚合为一条摘要事件，不必逐条写入

### 4.4 `metrics` 设计

#### 指标清单

| 指标名 | 类型 | 标签 | 用途 |
|---|---|---|---|
| `rss_feed_fetch_total` | counter | `category`, `status` | 拉取计数 |
| `rss_feed_fetch_duration_seconds` | histogram | `category` | 拉取耗时 |
| `rss_entry_discovered_total` | counter | `category` | 发现条目数 |
| `rss_entry_dedup_total` | counter | `category`, `layer` | 去重命中数 |
| `rss_extract_duration_seconds` | histogram | `strategy` | 正文提取耗时 |
| `rss_ai_call_total` | counter | `model`, `status` | AI 调用计数 |
| `rss_ai_call_duration_seconds` | histogram | `model` | AI 调用耗时 |
| `rss_publish_total` | counter | `category`, `status` | 发布计数 |
| `rss_lease_active` | gauge | `table` | 当前活跃 lease 数 |
| `rss_lease_reclaimed_total` | counter | `table` | lease 回收计数 |
| `rss_db_query_duration_seconds` | histogram | `operation` | 数据库操作耗时 |

#### 暴露方式

- `config.observability.enable_metrics = true` 时启动 HTTP endpoint
- 默认绑定 `127.0.0.1:9090/metrics`
- Prometheus 文本格式
- 首版可选实现，不作为 Phase 1 阻塞项

## 5. `doctor` 命令的健康检查

`doctor` 是宪法要求的正式 CLI 命令，不是辅助脚本。它覆盖以下检查项：

| 检查项 | 方法 | 通过条件 |
|---|---|---|
| 配置文件存在且合法 | 执行完整校验 | 零错误 |
| 数据库可连接 | 执行 `SELECT 1` | 成功返回 |
| 数据库 migration 版本 | 对比代码内嵌版本 | 一致 |
| `OPENAI_API_KEY` 有效 | 对 `{base_url}/chat/completions` 发送最小请求（1 token 上限，`stream=false`，messages 为单条 `ping`）| HTTP 200 且响应体可解析为合法 chat completion JSON；不校验 `/models` 端点（部分 OpenAI-compatible 端点不提供该路由）|
| `GITHUB_TOKEN` 有效 | 调用 `GET /user` | 200 |
| `RSSHUB_BASE_URL` 可达 | HTTP GET 首页 | 2xx |
| 时区数据可用 | 解析 `target_timezone` | 成功 |
| 磁盘空间 | 检查数据库目录 | > 100 MB |
| 过期 lease 数量 | 查询过期未回收 | 0（或报告数量）|
| 永久失败积压 | 查询 `state='failed'` 行数 | 报告数量 |

输出格式：

```text
[OK]   Configuration valid
[OK]   Database connection (SQLite, WAL mode)
[OK]   Migration version: 0003 (up to date)
[OK]   OpenAI API key valid (model: gpt-4o-mini)
[WARN] GitHub token: not configured (publish will fail)
[OK]   RSSHub base URL: https://rsshub.example.com (reachable)
[OK]   Timezone: Asia/Shanghai
[OK]   Disk space: 2.3 GB free
[WARN] 3 expired leases pending reclaim
[INFO] 12 permanently failed entries
```

exit code：全部 OK → 0；存在 WARN → 0；存在 FAIL → 1。

## 6. 六问检查清单

宪法 §5.1 要求每条核心流程回答六问。以下是模板，各流程的具体回答在 [state-machine](./state-machine.md) 中完成：

1. **成功路径**：正常完成时的状态转移和副作用
2. **失败条件**：哪些错误会触发此路径失败
3. **错误传播**：错误如何从能力层到达 runtime 到达 CLI
4. **重试边界**：`max_attempts` 是多少，超出后进入什么终态
5. **用户可见结果**：CLI 输出什么、exit code 是什么
6. **观测与验证方式**：哪些 span / event / metric 被记录，如何验证

## 7. 与宪法的对齐检查

- §5.1 失败路径：三层错误枚举 + `ClassifiedError` trait + `is_retryable` ✓
- §5.2 可观测性：`tracing` span 层次 + `run_events` 持久化 + `metrics` ✓
- §5.3 验证方式：`doctor` 命令 + 六问清单模板 ✓
- §3.4 单一真相源：`run_events` 是事件持久化的唯一真相源 ✓
- §5.4 版本责任：`config_version_changed` 事件关联 `rule_versions` ✓
