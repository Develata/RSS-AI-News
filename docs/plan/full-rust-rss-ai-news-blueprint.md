# RSS-AI-News 全 Rust 工程蓝图

## 1. 蓝图定位

本蓝图定义 `RSS-AI-News` 新 Rust 仓库的终局工程方案。它不是概念草图，而是可直接指导后续仓库初始化、模块落地、migration 设计、CLI 设计与任务拆解的工程蓝图。

本蓝图严格服从工程宪法，并以以下优先级作取舍：

1. 正确性 / 功能实现
2. 可用性 / 使用体验
3. 根基兼容性
4. 可维护性 / 可诊断性
5. 性能
6. 内存占用
7. 外存占用
8. 其它因素

## 2. 系统本体

### 2.1 系统定义

系统是一个内容处理流水线，用于：

- 从 RSS / Atom / RSSHub / JSON feed 发现内容
- 对内容做去重、正文提取与结构化存储
- 对文章执行 AI 分析
- 基于选稿结果生成日报
- 将日报落地到本地和 / 或发布到 GitHub

### 2.2 本体对象

核心对象固定为：

- `FeedSource`
- `FeedEntry`
- `Article`
- `ArticleAiResult`
- `PublishRecord`
- `PublishItem`
- `RawArtifact`

系统所有流程、模块、状态、存储模型都围绕这些对象组织。

## 3. 需求定义

### 3.1 功能需求

#### A. 输入源能力

必须支持：

- RSS
- Atom
- RSSHub 生成的 RSS
- 特定 JSON feed

必须支持：

- 多分类配置
- 每分类多源配置
- `{RSSHUB}` 占位符运行时替换
- source 启停与优先级

#### B. 抓取与发现能力

必须支持：

- HTTP / HTTPS 请求
- 代理
- 超时
- 重试
- 条件请求（ETag / Last-Modified）
- 发布时间窗口过滤
- link / guid 标识提取

#### C. 去重与正文能力

必须支持：

- 第一层：`source_id + feed_entry_uid` 去重
- 第二层：`normalized_link` 去重
- 第三层：`content_hash` 去重
- 详情页 HTML 抓取
- 多策略正文提取
- summary fallback

#### D. AI 能力

必须支持：

- OpenAI-compatible Chat API
- 分类级 prompt
- 分类级最大输入长度
- 摘要 / 标签 / 分数 / keep-filter 决策
- AI 关闭模式
- 无 AI 发布降级模式

#### E. 发布能力

必须支持：

- 按分类、按日期归档的 Markdown 报告
- excerpt 生成
- 本地文件输出
- GitHub 批量提交
- 发布快照与可重建报告

#### F. 运维能力

必须支持：

- `doctor`
- `ingest`
- `ai-run`
- `publish`
- `replay`
- `backfill`
- `rebuild-report`
- `reindex`
- `migrate`
- `validate-config`

### 3.2 非功能需求

#### 正确性与稳定性

- 所有核心流程必须可恢复
- 所有核心对象必须有单一真相源
- 所有重复执行的流程必须幂等或显式去重

#### 使用体验

- 默认 CLI 语义清晰
- 错误提示能定位到配置、网络、提取、AI、发布等层次
- 用户能理解“为什么某篇文章未入日报”

#### 根基兼容性

- 默认运行环境兼容 CA 证书体系
- 默认运行环境兼容 tzdata
- TLS / HTTP 行为稳定
- GitHub 发布接口通过适配层调用
- 关键依赖优先选择稳定生态

#### 性能与资源目标

- 常态抓取链低内存有界
- 所有队列固定容量
- 外部 payload 默认不全量常驻内存
- raw artifact 保留策略可配置

## 4. 骨架设计

### 4.1 四层架构 + 对象层

#### 第一层：交互壳层

承载：

- CLI
- 后续可能的 Web / TUI / GUI

职责：

- 输入采集
- 参数转译
- 结果呈现

#### 第二层：指令接口层

承载：

- 标准命令语义
- 参数合法性校验
- action -> intent 转换

职责：

- 把用户动作转成结构化意图
- 生成 state delta 请求

#### 第三层：流程协调层

承载：

- ingest orchestration
- ai-run orchestration
- publish orchestration
- replay / backfill / reindex orchestration

职责：

- 编排顺序
- 推进状态
- 控制补偿、中止和错误传播

#### 第四层：能力执行层

承载：

- 网络抓取
- 正文提取
- AI 调用
- 存储
- 本地文件输出
- GitHub 发布

#### 第五层：对象层

对象族：

- `config::*`
- `sql::*`
- `data::*`
- `artifact::*`

### 4.2 系统主链路

```text
FeedSource
  -> FeedResponse
  -> FeedEntryMeta
  -> DedupDecision
  -> ArticleFetchTask
  -> RawArticleContent
  -> ExtractedArticle
  -> PersistedArticle
  -> AiTask
  -> AiResult
  -> PublishSnapshot
  -> PublishedArtifact
```

### 4.3 核心不变量

1. 去重前不得抓取详情页正文
2. 任一队列都必须有固定容量
3. 入库后不得依赖进程内对象恢复状态
4. AI 任务只从数据库领取
5. 发布必须先冻结快照，再渲染，再推送
6. 所有并行任务都必须 claim + lease
7. 所有外部输入**必须具备 replay 能力**：系统设计层面为 feed payload、HTML payload、AI raw response 留出 artifact 通道；实际是否持久化由 `config.artifact.retention_policy` 控制（默认 `on_failure`，调试时可设 `always`）。宪法要求的是"能力常备"，不是"默认全量保留"
8. 核心对象、核心状态、核心配置必须有单一真相源
9. 所有核心流程都必须显式定义失败路径、观测点和验证方式

## 5. 状态模型

> 本节仅列出各状态机的状态集合与高层语义。完整 transition 表（触发者、前置条件、原子性、失败分支）见 [state-machine](../design/state-machine.md)。状态值在数据库中以 snake_case 字符串存储，对应的 Rust enum 变体使用 PascalCase。

### 5.1 FeedEntry 状态机

承载"发现 → 去重 → 抓取 → 提取 → 入库"。

状态集合（snake_case 持久化值）：

- `discovered` — 刚被 ingest 发现
- `dedup_skipped` — 一/二/三层去重命中（软终态）
- `pending_fetch` — 去重通过，等待正文抓取
- `fetching` — worker 正在抓详情页 HTML
- `extracting` — HTML 已抓到，正在提取正文
- `persisted` — 正文入库（成功终态）
- `fallback_persisted` — 正文抓取失败，但 summary fallback 入库（软终态）
- `failed` — 永久失败（失败终态）

### 5.2 Article 与 AiResult 双状态机

`articles.state` 与 `article_ai_results.state` 是两条相关但独立的状态机；一篇 `articles` 可对应多行 `article_ai_results`（不同 prompt / 协议 / 模型版本）。

#### 5.2.1 `articles.state`

- `persisted` — 正文入库，已准备好接受 AI
- `ai_pending` — 已创建至少一行 `article_ai_results` 任务
- `ai_done` — 至少一行 `article_ai_results.state='succeeded'`
- `ready_for_publish` — 符合发布条件
- `publish_skipped` — AI 判定过滤或不符发布条件（软终态）
- `published` — 被 `publish_items` 引用且 `publish_records` 成功（成功终态）
- `retired` — 软删除（首版预留）

`articles` 本身无 lease 字段；阶段推进由 `runtime` 在 `article_ai_results` / `publish_records` 更新的同一事务内同步 UPDATE。

#### 5.2.2 `article_ai_results.state`

- `pending` — 已创建任务行，等待 claim；也是 retryable 失败后回落的状态
- `running` — worker 已 claim，正在调用 AI
- `succeeded` — AI 调用成功且 JSON 解析成功（成功终态）
- `permanent_failed` — 重试耗尽或永久错误（失败终态）
- `filtered` — AI 明确判定 `keep_decision=0`（软终态）

retryable 失败不作为独立持久状态（见 [state-machine §4.2](../design/state-machine.md)）。

### 5.3 Publish 状态机

承载发布批次的"冻结 → 渲染 → 本地落盘 → 远程推送"全过程。

- `pending` — 已创建 `publish_records` 行，等待选稿冻结
- `snapshot_frozen` — 已写入 `publish_items`，内容冻结
- `rendered` — Markdown 已渲染（无持久副作用，可回放）
- `stored_local` — 本地落盘成功
- `published_remote` — 远程推送成功（成功终态）
- `failed` — 失败终态

## 6. 模块清单

### 6.1 根二进制 crate `rss-ai-news`

> 命名说明：W1 实施时把旧方案的 `app` crate 上提到根目录，根 `Cargo.toml` 直接声明 binary `rss-ai-news`，其余子模块全部下沉到 `crates/`。本节标题保留概念名"主二进制入口"，文中引用按现状使用 `rss-ai-news`。

职责：

- 主二进制入口
- 初始化配置、日志、数据库、runtime

### 6.2 `cli`

职责：

- 命令解析
- 参数验证
- 输出格式控制

### 6.3 `domain`

职责：

- 领域对象
- 状态机 enum
- DTO 契约
- link 规范化规则

### 6.4 `config`

职责：

- `.env`
- `app.toml`
- `categories/*.toml`
- schema version 校验

### 6.5 `runtime`

职责：

- 流程协调
- use-case 编排
- claim / lease 生命周期管理

### 6.6 `storage`

职责：

- schema
- migration
- repository
- 原子 claim / lease 更新

### 6.7 `feed`

职责：

- source 抓取
- RSS / Atom / JSON 解析
- 轻量条目规范化

### 6.8 `extractor`

职责：

- 详情页 HTML 抓取
- 多策略正文提取
- content hash
- fallback 处理

### 6.9 `ai`

职责：

- AI client
- prompt 组装
- 输入裁剪
- 输出解析
- version 记录

### 6.10 `report`

职责：

- 选稿
- excerpt 生成
- Markdown 渲染
- 发布快照固化

### 6.11 `publish`

职责：

- 本地输出
- GitHub 输出
- 发布状态推进

### 6.12 `observability`

职责：

- tracing
- metrics
- health check
- 关键事件结构化记录

## 7. 数据模型

### 7.1 持久对象

必须存在：

- `feed_sources`
- `feed_entries`
- `articles`
- `article_ai_results`
- `publish_records`
- `publish_items`

必须存在：

- `raw_artifacts`
- `rule_versions`
- `run_events`

> 字段级设计见 [storage-schema](../design/storage-schema.md)。状态机转移规则见 [state-machine](../design/state-machine.md)。

### 7.2 单一真相源映射

- Source 真相源：`feed_sources`
- FeedEntry 真相源：`feed_entries`
- Article 真相源：`articles`
- AI 真相源：`article_ai_results`
- Publish 真相源：`publish_records` + `publish_items`
- 配置真相源：结构化配置对象 + version

## 8. 调用关系

### 8.1 `ingest`

1. 壳层接收命令
2. 指令层解析参数
3. 流程协调层遍历可用 sources
4. `feed` 拉取与解析
5. `storage` 做 entry 幂等插入 / 去重判定
6. `runtime` 生成正文抓取任务
7. `extractor` 抓 HTML 并提正文
8. `storage` 写入 `articles`，`articles.state='persisted'`

### 8.2 `ai-run`

1. `runtime` 为 `articles.state='persisted'` 的文章生成 `article_ai_results` 任务行（同事务将 `articles.state` 推进到 `ai_pending`）
2. `runtime` 按 claim + lease 领取 `article_ai_results.state='pending'` 批次
3. `ai` 读取分类 prompt，组装输入
4. `ai` 调接口并解析结构化输出
5. `storage` 更新 `article_ai_results`，事务内同步推进 `articles.state` 至 `ai_done` / `ready_for_publish` / `publish_skipped`

### 8.3 `publish`

1. `runtime` 查询满足条件的 article
2. `report` 选择并排序 article
3. `report` 冻结 `PublishSnapshot`
4. `report` 渲染 Markdown
5. `publish` 输出本地文件
6. `publish` 视目标推送 GitHub
7. `storage` 更新 `publish_records`

### 8.4 `replay`

1. 选择 artifact
2. 根据 artifact 类型进入 feed / extractor / ai 对应重放入口
3. 输出解析结果、差异和错误上下文

### 8.5 `backfill`

1. 选择历史对象范围
2. 领取待补跑任务
3. 重跑 extractor 或 ai
4. 更新状态与版本信息

### 8.6 `rebuild-report`

1. 根据 `publish_record_id` 读取 `publish_items`
2. 复用快照数据重建 Markdown
3. 对比原始 artifact 或重新输出

## 9. 失败路径

### 9.1 Feed 拉取失败

- 记录 source 失败次数和时间
- 本 source 本轮结束
- 不影响其他 source

### 9.2 正文提取失败

- 优先回退 summary
- 若 summary 也不可用，则标记失败
- 保留 replay 所需上下文

### 9.3 AI 失败

- 区分可重试与永久失败
- 不影响抓取主链路
- 发布可视策略决定是否跳过

### 9.4 发布失败

- 保留已冻结 `publish_items`
- 保留已落盘 Markdown（若已到 `stored_local`）
- 将 `publish_records.state` 置为 `failed`
- 支持后续重试（幂等 key 策略见状态机 §5.3）

## 10. 配置入口

### 10.1 配置分层

- `app.toml`
- `categories/*.toml`
- `.env`

### 10.2 配置原则

- 结构化配置只读
- schema version 必须存在
- 配置校验失败立即退出
- 不允许静默 fallback 到危险默认值

## 11. 生命周期与退出路径

### 11.1 Source

- create
- enable
- disable
- archive

### 11.2 FeedEntry

- discover
- dedup skip / persist
- archive
- cleanup

### 11.3 RawArtifact

- create
- retain by policy
- archive / delete by TTL

### 11.4 PublishSnapshot

- freeze
- render
- publish
- archive

## 12. 扩展点与替换点

### 12.1 扩展点

- feed parser 实现
- extractor 策略
- AI provider
- publisher target
- excerpt 策略

### 12.2 替换点

- `HttpClient`
- `AiClient`
- `PublisherTarget`
- `TimezoneProvider`
- `LinkNormalizer`

## 13. 性能关键路径

### 13.1 关键路径

- source 拉取
- HTML 抓取
- 正文提取
- AI 网络调用
- 发布渲染与远端推送

### 13.2 优化原则

- 先保证正确性与可诊断性
- 再做分层限流
- 再做资源调优

## 14. 交付约束

### 14.1 生产镜像策略

- multi-stage build
- release 二进制
- runtime 保留 CA 证书与 tzdata
- 默认非 root 用户
- 提供生产镜像和调试镜像双产物

### 14.2 不默认追 `scratch`

终局目标是稳定优先的极简运行时，而不是为了极限压缩牺牲根基兼容性和维护体验。

### 14.3 调度模型：单次执行 CLI，调度外置

本项目的二进制是 **single-shot CLI**：每次调用执行一遍指定阶段（`ingest` / `ai-run` / `publish` / `run`）后即退出。不内置守护进程、不内置定时器、不常驻。

**理由**：

- 可观测性：每次运行对应一个 `run_id`，日志、metrics、run_events 天然按执行边界切分
- 可恢复性：崩溃后只需重新调用，lease 机制自动回收未完成任务
- 可调度性：宿主（cron / systemd timer / docker compose `restart: on-failure` / k8s CronJob / GitHub Actions schedule）比自建调度器更稳定
- 可测试性：集成测试直接调用二进制即可，不需要拉起常驻进程

**调度职责边界**：

| 关心的问题 | 由谁承担 |
|---|---|
| 何时触发一次运行 | 宿主（cron / CronJob / Actions）|
| 并发执行控制 | 数据库 lease（`crates/storage`）|
| 任务重试 | lease 过期回收 + `attempt_count` |
| 失败报警 | 宿主（cron 日志 / Prometheus Alertmanager）|
| 配置变更生效 | 下一次 CLI 调用自动读取 |

**不允许**：在 Rust 二进制内部引入 tokio interval、cron 解析器、或任何形式的"等待下一个周期"循环。任何这类需求都改写为宿主调度 + 单次 CLI。

唯一例外：`ingest` / `ai-run` 内部的批次处理循环（处理当前批次直到 `pending` 清空或达到 `batch_size * max_batches_per_run`）。这是单次运行内的工作分片，不是跨运行的调度。

## 15. 实施阶段

### Phase 0：文档冻结

- [工程宪法](../constitution.md)
- [设计哲学](../design/design-philosophy.md)
- [宪法落地对齐](../design/engineering-constitution-alignment.md)
- [Python 历史教训](../design/python-legacy-lessons.md)
- [存储 schema](../design/storage-schema.md)
- [状态机](../design/state-machine.md)
- [配置 schema](../design/config-schema.md)
- [内部 DTO 契约](../design/internal-dto-contracts.md)
- [CLI 语义](../design/cli-semantics.md)
- [错误模型与可观测性](../design/error-and-observability.md)
- [回放与 Artifact](../design/replay-and-artifacts.md)
- [依赖选型](../design/dependency-choices.md)
- 本蓝图

### Phase 1：最小闭环

- workspace 初始化
- config + storage + feed + runtime + cli + app
- SQLite
- ingest 闭环

### Phase 2：正文提取

- extractor
- fallback
- replay(html)

### Phase 3：AI 闭环

- ai crate
- AI 状态机
- replay(ai)
- backfill(ai)

### Phase 4：发布闭环

- report
- publish
- publish snapshot
- rebuild-report

### Phase 5：硬化

- metrics
- doctor
- reindex
- PostgreSQL
- 调试镜像

## 16. 判定

只有当某项实现既符合本蓝图，又不违反工程宪法时，才允许进入主干。  
任何局部实现若需要修改总骨架，必须先进入骨架变更分析与审批流程。
