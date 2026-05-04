# 存储 Schema 设计

## 1. 定位

本文档是 Rust 版存储层的实现级契约。它不是 migration SQL 本身，但所有 migration 都必须忠于本文档的字段、索引、约束与关系。与本文档不一致的 migration 不得进入主干。

它回答以下问题：

- 每张本体对象表有哪些字段、类型、可空性
- 哪些字段构成单一真相源的核心，不允许通过规定 transition 之外的途径修改
- claim / lease 如何在 SQL 层原子完成
- 幂等与去重的边界在哪
- migration 如何演进，旧数据如何迁移与回退

与之配套的状态机行为见 [state-machine](./state-machine.md)。

## 2. 总原则

### 2.1 真相源边界

每个本体对象都有且只有一张真相源表。任何"投影表"、"缓存表"、"物化视图"都不是真相源，不得作为写入目标。

| 本体对象 | 真相源表 |
|---|---|
| `FeedSource` | `feed_sources` |
| `FeedEntry` | `feed_entries` |
| `Article` | `articles` |
| `ArticleAiResult` | `article_ai_results` |
| `PublishRecord` | `publish_records` |
| `PublishItem` | `publish_items` |
| `RawArtifact` | `raw_artifacts` |
| `RuleVersion` | `rule_versions` |
| `RunEvent` | `run_events` |

### 2.2 版本责任

所有 schema 变更必须是一份 migration。migration 文件命名 `NNNN_short_name.up.sql` / `NNNN_short_name.down.sql`，NNNN 四位递增。禁止原地修改已合入的 migration。

每张表带两个审计列：

- `created_at` — 行创建时间，UTC，`DEFAULT CURRENT_TIMESTAMP`
- `updated_at` — 行最近修改时间，UTC，应用层或触发器维护

### 2.3 时间与时区

- 数据库内所有时间列存 UTC
- 时区转换只在 `config` / `report` / `cli` 边界执行
- SQLite 使用 `TEXT` 存 RFC3339 UTC，**唯一允许的字面形式**为 `YYYY-MM-DDTHH:MM:SS.fffZ`（`T` 与 `Z` 必填，三位毫秒必填，零毫秒写 `.000Z`）。该格式同时是 ISO8601 子集，并保证 SQLite 文本排序等价于时间排序
- 所有时间列的 serializer 必须使用同一格式串，parser 必须能往返还原相同字面值；契约测试覆盖：（a）排序与时间序一致；（b）`format → parse → format` 字面相等；（c）拒绝带空格分隔符或缺 `Z` 的旧格式
- PostgreSQL 使用 `TIMESTAMPTZ`，由驱动负责 RFC3339 ↔ binary 互转；不直接落地文本
- Rust 侧 DB 读写使用 `time::OffsetDateTime`（UTC-only），跨时区业务逻辑使用 `jiff::Timestamp`，二者在边界显式互转（见 [dependency-choices §2.5](./dependency-choices.md)）

### 2.4 主键策略

- 所有表默认使用 64 位自增整数主键 `id`
- 跨机器分布式场景下，保留将 `id` 升级为 ULID / UUIDv7 的迁移能力
- 对外暴露的稳定标识符不使用 `id`，而使用业务键（`feed_entries.feed_entry_uid`、`articles.content_hash`、`publish_records.idempotency_key`）

### 2.5 空值与默认值

- 可空列必须有明确业务理由（典型："AI 还没跑"、"正文还没抓"）
- 所有状态列非空，且有 `DEFAULT` 对应初始状态字符串
- 所有计数器列非空，`DEFAULT 0`

### 2.6 软删除与退出路径

- 核心对象不做物理删除，改用状态字段标记（`FeedSource.status='archived'`）
- `raw_artifacts` 允许按 TTL 物理删除（见 §9）
- `publish_records` / `publish_items` 永久保留

## 3. 数据库引擎选择

### 3.1 首版 SQLite

理由：

- 单文件、零运维
- 与 Python 版运行目标一致，便于并行验证
- 本项目典型负载在单机范围内

运行期 PRAGMA（由 `storage` crate 启动时统一设置）：

- `PRAGMA journal_mode=WAL`
- `PRAGMA foreign_keys=ON`
- `PRAGMA busy_timeout=5000`
- `PRAGMA synchronous=NORMAL`

### 3.2 PostgreSQL 是一等替换目标

理由：

- 多 worker / 多机扩展
- 更强的并发与锁粒度
- 服务器级运维场景

约束：

- 所有 SQL 必须在 SQLite 与 PG 两个方言下都能工作，或通过 `sqlx` 编译期方言分叉
- 不使用 SQLite 专有的 `AUTOINCREMENT` 语义，改用 `INTEGER PRIMARY KEY` 或 `BIGSERIAL`
- 布尔统一映射为 domain 的 `bool`

### 3.3 抽象边界

- `storage` crate 对外暴露 `Repository` trait，不暴露 `sqlx::Pool` 本身
- 每张表一个 `*Repository` trait 和具体实现
- `runtime` 层只依赖 `Repository` trait
- 引擎选择通过 `config.database.driver` 决定，运行时注入对应具体类型

## 4. 表清单

> 下列字段表列出"列名 / 类型 / 约束 / 说明"。SQLite 与 PG 的类型差异在 migration 中通过条件分叉处理，本文档用通用表达（`TIMESTAMPTZ` 代指时间列）。

### 4.1 `feed_sources`

订阅源真相源表。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | 内部主键 |
| `category_key` | `TEXT` | NOT NULL | 对应 `config::category` 的 key |
| `source_key` | `TEXT` | NOT NULL | 源 key，分类内唯一 |
| `display_name` | `TEXT` | NOT NULL | UI 展示名 |
| `feed_url` | `TEXT` | NOT NULL | 规范化后的 URL |
| `feed_kind` | `TEXT` | NOT NULL | `rss` / `atom` / `json_feed` / `rsshub` |
| `status` | `TEXT` | NOT NULL, DEFAULT `'active'` | `active` / `paused` / `archived` |
| `priority` | `INTEGER` | NOT NULL, DEFAULT 100 | 调度优先级，越小越优先 |
| `etag` | `TEXT` | NULL | 上次响应 `ETag` |
| `last_modified` | `TEXT` | NULL | 上次响应 `Last-Modified` |
| `last_fetched_at` | `TIMESTAMPTZ` | NULL | 上次尝试拉取时间 |
| `last_success_at` | `TIMESTAMPTZ` | NULL | 上次成功拉取时间 |
| `consecutive_failures` | `INTEGER` | NOT NULL, DEFAULT 0 | 连续失败次数 |
| `last_error` | `TEXT` | NULL | 最近一次错误摘要 |
| `last_error_kind` | `TEXT` | NULL | 错误分类枚举 |
| `config_version` | `INTEGER` | NOT NULL | `rule_versions.id` |
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |
| `updated_at` | `TIMESTAMPTZ` | NOT NULL | - |

约束与索引：

- `UNIQUE (category_key, source_key)`
- `INDEX ON (status)`
- `INDEX ON (category_key, priority)`
- `FOREIGN KEY (config_version) REFERENCES rule_versions(id)`

### 4.2 `feed_entries`

feed 条目真相源表。承载"发现 → 抓取 → 提取 → 入库"状态机。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | - |
| `source_id` | `INTEGER` | NOT NULL | `feed_sources.id` |
| `feed_entry_uid` | `TEXT` | NOT NULL | feed 内原始 `guid` / `id` |
| `normalized_link` | `TEXT` | NOT NULL | 规范化 URL |
| `link_hash` | `TEXT` | NOT NULL | `normalized_link` 的 sha256 |
| `title_raw` | `TEXT` | NOT NULL | 原始标题 |
| `summary_raw` | `TEXT` | NULL | 原始摘要 |
| `published_at` | `TIMESTAMPTZ` | NULL | feed 声明的发布时间 |
| `discovered_at` | `TIMESTAMPTZ` | NOT NULL | 系统首次发现时间 |
| `state` | `TEXT` | NOT NULL, DEFAULT `'discovered'` | 见 [state-machine §3](./state-machine.md) |
| `dedup_decision` | `TEXT` | NULL | 该行实际落入的分支：`fresh`（前两层都通过，成功 INSERT 并继续走抓取）/ `hash_dup`（第三层正文内容去重命中，行已 INSERT 后被标记为 `dedup_skipped`）。前两层（UID / link）去重不产生 `feed_entries` 行，因此不会取 `uid_dup` / `link_dup` 值（DTO `DedupDecision` 枚举在 `runtime` 内仍区分四种，仅不持久化到本表）。|
| `article_id` | `INTEGER` | NULL | `articles.id`（正文入库后回填）|
| `lease_owner` | `TEXT` | NULL | 当前 claim 的 worker id |
| `lease_expires_at` | `TIMESTAMPTZ` | NULL | 租约过期时间 |
| `attempt_count` | `INTEGER` | NOT NULL, DEFAULT 0 | - |
| `last_error` | `TEXT` | NULL | - |
| `last_error_kind` | `TEXT` | NULL | - |
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |
| `updated_at` | `TIMESTAMPTZ` | NOT NULL | - |

约束与索引：

- `UNIQUE (source_id, feed_entry_uid)` — 第一层去重
- `INDEX ON (link_hash)` — 第二层去重查询
- `INDEX ON (state, lease_expires_at)` — claim 扫描
- `INDEX ON (source_id, discovered_at)` — 按源回查
- `FOREIGN KEY (source_id) REFERENCES feed_sources(id)`
- `FOREIGN KEY (article_id) REFERENCES articles(id)` ON DELETE SET NULL

### 4.3 `articles`

文章正文真相源表。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | - |
| `content_hash` | `TEXT` | NOT NULL | 正文 sha256，第三层去重 |
| `canonical_link` | `TEXT` | NOT NULL | 最终定版 URL |
| `title` | `TEXT` | NOT NULL | 清洗后标题 |
| `body_text` | `TEXT` | NOT NULL | 纯文本正文 |
| `body_html_artifact_id` | `INTEGER` | NULL | 指向 `raw_artifacts`，可空 |
| `extractor_strategy` | `TEXT` | NOT NULL | `readability` / `rule` / `summary_fallback` |
| `extractor_version` | `INTEGER` | NOT NULL | `rule_versions.id` |
| `content_quality` | `TEXT` | NOT NULL | `high` / `medium` / `fallback` |
| `word_count` | `INTEGER` | NOT NULL, DEFAULT 0 | - |
| `origin_feed_entry_id` | `INTEGER` | NOT NULL | `feed_entries.id` |
| `state` | `TEXT` | NOT NULL, DEFAULT `'persisted'` | 见状态机 §4 |
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |
| `updated_at` | `TIMESTAMPTZ` | NOT NULL | - |

约束与索引：

- `UNIQUE (content_hash)` — 第三层内容去重
- `INDEX ON (canonical_link)`
- `INDEX ON (state, created_at)`
- `FOREIGN KEY (origin_feed_entry_id) REFERENCES feed_entries(id)`
- `FOREIGN KEY (extractor_version) REFERENCES rule_versions(id)`
- `FOREIGN KEY (body_html_artifact_id) REFERENCES raw_artifacts(id) ON DELETE SET NULL`

### 4.4 `article_ai_results`

AI 结果真相源表。一篇文章允许多行（不同 prompt / 协议 / 模型）。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | - |
| `article_id` | `INTEGER` | NOT NULL | `articles.id` |
| `prompt_version` | `INTEGER` | NOT NULL | `rule_versions.id` |
| `output_schema_version` | `INTEGER` | NOT NULL | `rule_versions.id` |
| `model_id` | `TEXT` | NOT NULL | e.g. `gpt-4o-mini` |
| `state` | `TEXT` | NOT NULL, DEFAULT `'pending'` | 见状态机 §4 |
| `summary` | `TEXT` | NULL | 成功后填 |
| `tags_json` | `TEXT` | NULL | JSON array |
| `importance_score` | `INTEGER` | NULL | 0–100 |
| `keep_decision` | `INTEGER` | NULL | 0/1 |
| `raw_response_artifact_id` | `INTEGER` | NULL | `raw_artifacts.id` |
| `tokens_in` | `INTEGER` | NULL | 本次调用输入 token 数（来自 API usage 字段）|
| `tokens_out` | `INTEGER` | NULL | 本次调用输出 token 数 |
| `cost_micro_usd` | `INTEGER` | NULL | 估算成本，单位为百万分之一美元（1 美元 = 10^6 micro_usd），避免浮点精度 |
| `latency_ms` | `INTEGER` | NULL | 从发请求到收完整响应的耗时（毫秒）|
| `lease_owner` | `TEXT` | NULL | - |
| `lease_expires_at` | `TIMESTAMPTZ` | NULL | - |
| `attempt_count` | `INTEGER` | NOT NULL, DEFAULT 0 | - |
| `last_error` | `TEXT` | NULL | - |
| `last_error_kind` | `TEXT` | NULL | - |
| `started_at` | `TIMESTAMPTZ` | NULL | - |
| `completed_at` | `TIMESTAMPTZ` | NULL | - |
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |
| `updated_at` | `TIMESTAMPTZ` | NOT NULL | - |

约束与索引：

- `UNIQUE (article_id, prompt_version, output_schema_version, model_id)` — 幂等四元组
- `INDEX ON (state, lease_expires_at)`
- `INDEX ON (article_id)`
- `FOREIGN KEY (article_id) REFERENCES articles(id)`
- `FOREIGN KEY (prompt_version) REFERENCES rule_versions(id)`
- `FOREIGN KEY (output_schema_version) REFERENCES rule_versions(id)`
- `FOREIGN KEY (raw_response_artifact_id) REFERENCES raw_artifacts(id) ON DELETE SET NULL`
- `CHECK (importance_score IS NULL OR (importance_score BETWEEN 0 AND 100))`（值域硬约束；与 DTO `Score0To100` newtype 一致）
- `CHECK (keep_decision IS NULL OR keep_decision IN (0, 1))`（布尔域硬约束）

`tokens_in` / `tokens_out` / `cost_micro_usd` 计量规则：

- `tokens_in` / `tokens_out`：仅在 AI API 返回 `usage.prompt_tokens` / `usage.completion_tokens` 时填入；未返回则保持 NULL（不强行估算）
- `cost_micro_usd`：**首版仅在 provider 直接返回成本字段时填入**（例如部分 OpenAI-compatible 聚合服务在 usage 中返回 `cost` 或等价字段）；本地不维护单价映射表。单价换算功能如需开启，由后续版本通过独立 `ai_pricing` 配置段引入，届时一并更新 config-schema
- 字段仅供成本分析，不参与业务逻辑；不构成状态转移条件
- 失败请求也应尝试填入（许多 API 在 4xx 响应中仍返回部分 usage）

### 4.5 `publish_records`

发布批次真相源表。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | - |
| `idempotency_key` | `TEXT` | NOT NULL | `{category}-{report_date}-{render_version}` |
| `category_key` | `TEXT` | NOT NULL | - |
| `report_date` | `TEXT` | NOT NULL | `YYYY-MM-DD`（目标时区日期）|
| `target_timezone` | `TEXT` | NOT NULL | IANA tz, e.g. `Asia/Shanghai` |
| `render_version` | `INTEGER` | NOT NULL | `rule_versions.id` |
| `selection_policy_version` | `INTEGER` | NOT NULL | `rule_versions.id` |
| `state` | `TEXT` | NOT NULL, DEFAULT `'pending'` | 见状态机 §5 |
| `snapshot_frozen_at` | `TIMESTAMPTZ` | NULL | - |
| `rendered_at` | `TIMESTAMPTZ` | NULL | - |
| `local_stored_at` | `TIMESTAMPTZ` | NULL | - |
| `remote_published_at` | `TIMESTAMPTZ` | NULL | - |
| `local_path` | `TEXT` | NULL | 本地产物路径 |
| `remote_target` | `TEXT` | NULL | e.g. `github://owner/repo/branch/path` |
| `commit_sha` | `TEXT` | NULL | 推送成功后填 |
| `lease_owner` | `TEXT` | NULL | - |
| `lease_expires_at` | `TIMESTAMPTZ` | NULL | - |
| `attempt_count` | `INTEGER` | NOT NULL, DEFAULT 0 | - |
| `last_error` | `TEXT` | NULL | - |
| `last_error_kind` | `TEXT` | NULL | - |
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |
| `updated_at` | `TIMESTAMPTZ` | NOT NULL | - |

约束与索引：

- `UNIQUE (idempotency_key)` — 发布防重
- `INDEX ON (category_key, report_date)`
- `INDEX ON (state, lease_expires_at)`
- `FOREIGN KEY (render_version) REFERENCES rule_versions(id)`
- `FOREIGN KEY (selection_policy_version) REFERENCES rule_versions(id)`

### 4.6 `publish_items`

发布项冻结表。`rebuild-report` 的全部依据。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | - |
| `publish_record_id` | `INTEGER` | NOT NULL | - |
| `position` | `INTEGER` | NOT NULL | 日报内顺序 |
| `article_id` | `INTEGER` | NOT NULL | - |
| `article_ai_result_id` | `INTEGER` | NULL | 直通路径 NULL，AI 路径 NOT NULL；语义见 [state-machine §4.1.3](./state-machine.md#413-ai-关闭--无-ai-发布降级) |
| `frozen_title` | `TEXT` | NOT NULL | - |
| `frozen_summary` | `TEXT` | NOT NULL | 直通时取 `articles.summary_raw`；AI 路径取 AI 摘要 |
| `frozen_tags_json` | `TEXT` | NOT NULL | 直通时固定为 `"[]"` |
| `frozen_score` | `INTEGER` | NULL | 与 `article_ai_result_id` 同 NULL；值域 0–100（见下方 CHECK）|
| `frozen_canonical_link` | `TEXT` | NOT NULL | - |
| `frozen_source_display_name` | `TEXT` | NOT NULL | - |
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |

约束与索引：

- `UNIQUE (publish_record_id, article_id)`
- `INDEX ON (publish_record_id, position)`
- `FOREIGN KEY (publish_record_id) REFERENCES publish_records(id) ON DELETE CASCADE`
- `FOREIGN KEY (article_id) REFERENCES articles(id)`
- `FOREIGN KEY (article_ai_result_id) REFERENCES article_ai_results(id)`（NULL 时外键无效）
- `CHECK ((article_ai_result_id IS NOT NULL AND frozen_score IS NOT NULL) OR (article_ai_result_id IS NULL AND frozen_score IS NULL))`（两列必须同时有值或同时为 NULL，避免半降级的脏数据）
- `CHECK (frozen_score IS NULL OR (frozen_score BETWEEN 0 AND 100))`（值域硬约束；与 DTO `Score0To100` newtype 一致）

### 4.7 `raw_artifacts`

原始输入留档。支持 replay 的核心依据。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | - |
| `kind` | `TEXT` | NOT NULL | `feed_payload` / `html_payload` / `ai_raw_response` |
| `artifact_key` | `TEXT` | NOT NULL | 业务层稳定 key，见 replay 文档 |
| `content_encoding` | `TEXT` | NOT NULL | `utf8` / `gzip` / `base64` |
| `storage_kind` | `TEXT` | NOT NULL | `inline` / `file` |
| `inline_body` | `BLOB` | NULL | 小于阈值时直接存表 |
| `file_path` | `TEXT` | NULL | 大于阈值时落外部文件 |
| `byte_size` | `INTEGER` | NOT NULL | 原始字节数 |
| `sha256` | `TEXT` | NOT NULL | 内容 hash |
| `retention_policy` | `TEXT` | NOT NULL | `always` / `on_failure` / `sampled` / `debug_only` |
| `expires_at` | `TIMESTAMPTZ` | NULL | TTL 到期时间。`NULL` 表示永不过期，仅当 `retention_policy='always'` 或其他无需 TTL 的保留策略使用；非 NULL 值由 runtime 在写入时按策略计算（与 [replay-and-artifacts §3.2](./replay-and-artifacts.md) 保持一致）|
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |

约束与索引：

- `UNIQUE (kind, artifact_key)` — 同 key 只保留一份
- `INDEX ON (expires_at)` — 清理扫描
- `INDEX ON (kind)`
- `CHECK ((storage_kind='inline' AND inline_body IS NOT NULL) OR (storage_kind='file' AND file_path IS NOT NULL))`

### 4.8 `rule_versions`

规则/协议版本注册表。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | - |
| `kind` | `TEXT` | NOT NULL | 见下 |
| `version_tag` | `TEXT` | NOT NULL | 语义版本或日期版本 |
| `description` | `TEXT` | NOT NULL | 人类可读说明 |
| `payload_sha256` | `TEXT` | NOT NULL | 规则内容 hash |
| `retired_at` | `TIMESTAMPTZ` | NULL | 退役时间 |
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |

`kind` 枚举：

- `config`
- `extractor`
- `prompt`
- `ai_output_schema`
- `render`
- `selection_policy`
- `link_normalizer`
- `content_hash`

约束与索引：

- `UNIQUE (kind, version_tag)`
- `INDEX ON (kind, retired_at)`

### 4.9 `run_events`

关键事件的持久化镜像。不是完整日志流，只记录会影响骨架判断的事件。

| 列名 | 类型 | 约束 | 说明 |
|---|---|---|---|
| `id` | `INTEGER` | PRIMARY KEY | - |
| `run_id` | `TEXT` | NOT NULL | 一次 CLI 执行的关联 id（ULID）|
| `trace_id` | `TEXT` | NULL | `tracing` 的 trace id |
| `stage` | `TEXT` | NOT NULL | 枚举：`ingest` / `extract` / `ai_run` / `publish` / `replay` / `backfill` / `rebuild_report` / `reindex` / `doctor`。持久化/日志字段统一用 snake_case；对应的 CLI 子命令是 kebab-case（`ai-run` / `rebuild-report` / `validate-config`），二者通过映射表转换，不在表里存 kebab-case |
| `severity` | `TEXT` | NOT NULL | `info` / `warn` / `error` / `critical` |
| `event_kind` | `TEXT` | NOT NULL | 业务事件枚举 |
| `target_kind` | `TEXT` | NULL | 关联对象类型，见下方枚举 |
| `target_id` | `INTEGER` | NULL | 关联对象 id |
| `message` | `TEXT` | NOT NULL | 摘要 |
| `context_json` | `TEXT` | NULL | 额外上下文 |
| `created_at` | `TIMESTAMPTZ` | NOT NULL | - |

`target_kind` 枚举（与本表 §4.1–§4.7 真相源对象一一对应）：

- `feed_source`
- `feed_entry`
- `article`
- `article_ai_result`
- `publish_record`
- `publish_item`
- `raw_artifact`

无关联对象时 `target_kind` 与 `target_id` 同时为 NULL（如 `run_started` / `migration_applied` 类事件）。

约束与索引：

- `INDEX ON (run_id)`
- `INDEX ON (stage, severity, created_at)`
- `INDEX ON (target_kind, target_id)`

## 5. Claim + Lease SQL 模板

### 5.1 领取一批 `feed_entries` 正文抓取任务

时间计算由 Rust 端完成，绑定到 SQL 的全部是绝对时间戳。

**绑定参数来源**（由 `storage` crate 调用前计算）：

- `:now = OffsetDateTime::now_utc()`
- `:lease_expires_at = :now + lease_duration`，其中 `lease_duration` 按调用阶段从 `[lease]` 段读取（见 [config-schema §4](./config-schema.md)）：
  - feed 抓取（§5.1 / §5.2 / §5.3 / §5.4）使用 `config.lease.fetch_duration_seconds`
  - AI 调用（§5.6）使用 `config.lease.ai_duration_seconds`
  - 发布阶段（§5.7）使用 `config.lease.publish_duration_seconds`
  - 调用方在准备 SQL 参数时把对应阶段的 `Duration` 加到 `:now`，SQL 模板只接收最终的绝对时间戳
- `:max_attempts`、`:batch_size`、`:owner` 由调用方显式提供

```sql
UPDATE feed_entries
SET
    state = 'fetching',
    lease_owner = :owner,
    lease_expires_at = :lease_expires_at,
    attempt_count = attempt_count + 1,
    updated_at = :now
WHERE id IN (
    SELECT id FROM feed_entries
    WHERE state = 'pending_fetch'
      AND (lease_expires_at IS NULL OR lease_expires_at < :now)
      AND attempt_count < :max_attempts
    ORDER BY discovered_at ASC
    LIMIT :batch_size
)
RETURNING id, source_id, normalized_link, link_hash, title_raw, discovered_at, attempt_count;
```

注意：

- **SQLite 最低支持版本：3.35.0（2021-03，`RETURNING` 引入版本）**。runtime 启动时 `doctor preflight` 通过 `SELECT sqlite_version()` 校验；低于 3.35 直接 fail-fast 退出（exit 78），不再提供低版本回退路径
- 禁止用"先 SELECT 后 UPDATE 不加锁"的方式实现——必然双抢
- 禁止在 SQL 中做时间加法（`:now + :lease_duration` 等写法不在 SQLite/PG 间可移植）；所有时间常量与偏移都必须在 Rust 端算好
- `owner` 推荐格式：`{hostname}-{pid}-{random_ulid}`

**PostgreSQL 方言差异**：

在 PG 下推荐改写子查询为 `FOR UPDATE SKIP LOCKED` 形式，避免高并发下多个 worker 阻塞在同一批候选行上：

```sql
UPDATE feed_entries
SET ...
WHERE id IN (
    SELECT id FROM feed_entries
    WHERE state = 'pending_fetch'
      AND (lease_expires_at IS NULL OR lease_expires_at < :now)
      AND attempt_count < :max_attempts
    ORDER BY discovered_at ASC
    LIMIT :batch_size
    FOR UPDATE SKIP LOCKED
)
RETURNING ...;
```

- SQLite 不支持 `FOR UPDATE SKIP LOCKED`，但 SQLite 写锁是库级串行化（WAL 模式下仍然写串行），`UPDATE ... WHERE id IN (SELECT ...)` 已经足够
- `crates/storage` 的 repository trait 用 `#[cfg(feature = "postgres")]` 分派两份 SQL，或运行时检测 `DATABASE_URL` 选择方言
- 本文档 §5.2–§5.7 的其它 UPDATE 本质是按主键 + `lease_owner` 的乐观锁，无需 `FOR UPDATE SKIP LOCKED`

### 5.2 释放一条 `feed_entry` 为成功

```sql
UPDATE feed_entries
SET
    state = 'persisted',
    article_id = :article_id,
    lease_owner = NULL,
    lease_expires_at = NULL,
    last_error = NULL,
    last_error_kind = NULL,
    updated_at = :now
WHERE id = :id AND lease_owner = :owner;
```

`WHERE lease_owner = :owner` 是时序一致性的关键：若 lease 已被 reclaim，本次 UPDATE 影响 0 行，调用方必须视为冲突。

### 5.3 释放为可重试失败

```sql
UPDATE feed_entries
SET
    state = 'pending_fetch',
    lease_owner = NULL,
    lease_expires_at = NULL,
    last_error = :err,
    last_error_kind = :err_kind,
    updated_at = :now
WHERE id = :id AND lease_owner = :owner;
```

### 5.4 释放为永久失败

```sql
UPDATE feed_entries
SET
    state = 'failed',
    lease_owner = NULL,
    lease_expires_at = NULL,
    last_error = :err,
    last_error_kind = :err_kind,
    updated_at = :now
WHERE id = :id AND lease_owner = :owner;
```

### 5.5 lease reclaim 扫描

后台周期任务。reclaim 必须既清 lease 字段、又把 `state` 回滚到对应 claim SQL 能匹配的状态，否则下一轮 claim（§5.1 / §5.6 只匹配 `pending_fetch` / `pending`）会看不见这些行。

**`feed_entries`**：

```sql
UPDATE feed_entries
SET
    state = 'pending_fetch',
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = :now
WHERE lease_expires_at IS NOT NULL
  AND lease_expires_at < :now
  AND state IN ('fetching', 'extracting');
```

**`article_ai_results`**：

```sql
UPDATE article_ai_results
SET
    state = 'pending',
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = :now
WHERE lease_expires_at IS NOT NULL
  AND lease_expires_at < :now
  AND state = 'running';
```

**`publish_records`**（§5.7 的多个 claim 方法各自按当前中间态 `:from` 匹配，因此 reclaim 只清 lease、不改 state）：

```sql
UPDATE publish_records
SET
    lease_owner = NULL,
    lease_expires_at = NULL,
    updated_at = :now
WHERE lease_expires_at IS NOT NULL
  AND lease_expires_at < :now
  AND state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local');
```

reclaim 不应触及 `attempt_count`：失败计数仍由 `release_*_failure` 控制，避免 reclaim 把"已尝试一次"的 lease 静默回滚为"未尝试"。

### 5.6 `article_ai_results` 的领取

`:lease_expires_at` 同 §5.1 由 Rust 端预先计算。

```sql
UPDATE article_ai_results
SET
    state = 'running',
    lease_owner = :owner,
    lease_expires_at = :lease_expires_at,
    attempt_count = attempt_count + 1,
    started_at = COALESCE(started_at, :now),
    updated_at = :now
WHERE id IN (
    SELECT id FROM article_ai_results
    WHERE state = 'pending'
      AND (lease_expires_at IS NULL OR lease_expires_at < :now)
      AND attempt_count < :max_attempts
    ORDER BY id ASC
    LIMIT :batch_size
)
RETURNING id, article_id, prompt_version, output_schema_version, model_id;
```

### 5.7 `publish_records` 的领取

发布任务的 claim 模式同上。不同点在于 `state` 可能是 `pending` / `snapshot_frozen` / `rendered` / `stored_local` 中的任一"中间未终结态"，claim 时按状态字段分别领取并推进。

## 6. 幂等与去重

### 6.1 三层去重

1. **第一层**：`feed_entries.UNIQUE(source_id, feed_entry_uid)` — feed 内 guid
2. **第二层**：`feed_entries.link_hash` + 等值查询，insert 前 SELECT 或 `ON CONFLICT` 忽略
3. **第三层**：`articles.UNIQUE(content_hash)` — 内容层面去重

### 6.2 发布幂等

`publish_records.UNIQUE(idempotency_key)` 是发布重试安全的保证。

`idempotency_key` 必须在"选稿即将冻结快照"之前计算完成并 INSERT；若返回唯一约束冲突，说明同一批次已经在进行或已完成。调用方根据当前 `state` 决定恢复策略（见 [state-machine §5](./state-machine.md)）。

### 6.3 AI 幂等

同一 `(article_id, prompt_version, output_schema_version, model_id)` 四元组只保留一行。重新跑需要 bump 任一版本号或 model_id——这是有意的严格约束：历史结果不可覆盖。

### 6.4 artifact 幂等

`raw_artifacts.UNIQUE(kind, artifact_key)` — 同 kind 同 key 只保留最新内容。覆盖写是 UPSERT，不允许追加历史。

## 7. Migration 策略

### 7.1 目录与命名

- 所有 migration 位于 `migrations/`
- 命名 `NNNN_short_name.up.sql` 与 `NNNN_short_name.down.sql`
- NNNN 从 `0001` 起递增

### 7.2 规则

- 已合入 main 的 migration 不得原地修改，只能新增 migration 纠正
- 每次 migration 必须同时提供 `up` 与 `down`，除非变更被显式标注为不可逆（需在 description 中说明）
- schema 破坏性变更必须先写数据迁移脚本，再写结构变更
- migration 执行由 `storage::migrate` 入口统一驱动
- `cli migrate --check` 不实际执行，只对比版本号

### 7.3 首版 migration

`0001_init.up.sql` 必须一次性建齐 §4 的全部 9 张表。理由：本项目没有"先上线后补表"的历史包袱，首版即终局骨架。

## 8. 失败路径

### 8.1 写入类失败

- 唯一约束冲突 → `StorageError::Conflict { table, key }`，`runtime` 按幂等语义处理
- 外键约束失败 → `StorageError::Integrity { table, reference }`，属于 bug，fail fast
- 连接断开 → `StorageError::Unavailable`（retryable）

### 8.2 读取类失败

- 不存在 → `Option::None`，不作为错误
- 超时 → `StorageError::Timeout`
- 解析失败 → `StorageError::Corruption`，写 `critical` 事件并阻止继续写入

### 8.3 锁争用

- SQLite `busy_timeout` 到点 → 可重试，外层退避 + 有限次重试
- PG 死锁检测 → sqlx 返回特定错误码，可重试

所有 `StorageError` 必须带 `retryable: bool`，`runtime` 据此决定后续行为。

## 9. 退出路径

### 9.1 `FeedSource`

- `paused`：停止调度，保留历史
- `archived`：停止调度，保留历史，不在 `doctor` 检查里报 warn
- 物理删除：不允许（会破坏外键级联语义）

### 9.2 `FeedEntry`

- `failed` 终态行在 TTL 到期后可被归档任务挪到冷表，首版不实现
- 物理删除仅限于数据破坏修复时的 admin 脚本

### 9.3 `Article`

- 软删除通过 `state = 'retired'`（首版可不启用，作为未来预留）
- 物理删除不允许

### 9.4 `RawArtifact`

- 按 `retention_policy` + `expires_at` 物理删除
- 后台任务执行，删除事件必须写 `run_events`

### 9.5 `PublishRecord` / `PublishItem`

- 物理删除不允许
- 需要发布新版本时，用新 `idempotency_key`（附加版本后缀）作为新批次；旧版本永久保留

## 10. 与宪法的对齐检查

- §3.4 单一真相源：每张表职责清晰，无并列真相源 ✓
- §5.4 版本责任：所有变更点都有 `*_version` 关联 `rule_versions` ✓
- §5.5 幂等：三层去重 + UNIQUE 约束 + claim 原子 UPDATE ✓
- §5.5 并发一致性：`lease_owner` + `lease_expires_at` + `WHERE lease_owner = :owner` 模式 ✓
- §5.1 失败路径：每条 SQL 关联的 StorageError 枚举已列 ✓
- §5.2 可观测性：`run_events` 内建 ✓
- §6.2 退出路径：`FeedSource.status` + `raw_artifacts.expires_at` 覆盖 ✓

与 [工程蓝图 §7](../plan/full-rust-rss-ai-news-blueprint.md) 的单一真相源映射一致。
