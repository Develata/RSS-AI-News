# 00 — 系统总览

本章是其余章节的统领：定义系统本体、主链路、四层架构和核心不变量。其它章节都假设
读者已读完本章。

## 1. 一句话定位

RSS-AI-News 是一个**单进程、一次性触发**的内容处理 CLI 管线：

```text
抓取 RSS / Atom / JSON Feed
  → 去重 + 抓正文 + 提取
  → AI 摘要 / 标签 / 评分 / 过滤
  → 渲染 Markdown 日报
  → 本地落盘 + GitHub 提交
```

由外部调度器（Docker scheduler 镜像 / cron / systemd timer / GHA / K8s CronJob）按需触发。
进程**不**内置 cron，**不**长期驻留。这是 [ADR-0001](../adr/0001-single-shot-cli-no-builtin-cron.md)
固化的边界。

## 2. 本体对象

系统骨架围绕固定本体展开。所有模块、状态机、表、命令都映射到这套对象上。

| 对象 | 角色 | 主表 |
|---|---|---|
| `FeedSource` | 订阅源（按分类配置） | `feed_sources` |
| `FeedEntry` | feed 中的一条目（去重前/后） | `feed_entries` |
| `Article` | 正文已入库的文章 | `articles` |
| `ArticleAiResult` | 单次 AI 分析结果（多版本） | `article_ai_results` |
| `PublishRecord` | 一次发布任务的状态机载体 | `publish_records` |
| `PublishItem` | `PublishRecord` 冻结时引用的具体文章 | `publish_items` |
| `RawArtifact` | feed / HTML / AI 原始输入留档 | `raw_artifacts` |
| `ReindexJob` | 规则版本升级任务（独立状态轮） | `reindex_jobs` |

**口径约定**（避免与旧文档同名概念混淆）：
- "PublishSnapshot" 不是独立对象，是 `PublishRecord` + 其 `PublishItem` 集合在某时刻的冻结读取
- "PublishedArtifact" 不是独立对象，发布产物（Markdown 文件 / commit SHA / 远端 URL）存在 `PublishRecord` 行内
- 真相源对象固定为上表 8 个，其它命名禁止与之并列

详细对象契约见 [05-storage.md](./05-storage.md)；状态机见 [08-state-machines.md](./08-state-machines.md)。

## 3. 主链路

```text
FeedSource
  -> FeedResponse        (网络抓取)
  -> FeedEntryMeta       (解析 + 三层去重)
  -> ArticleFetchTask    (任务化)
  -> RawArticleContent   (详情页 HTML)
  -> ExtractedArticle    (策略链提取)
  -> PersistedArticle    (Article 入库)
  -> AiTask              (article_ai_results pending 行)
  -> AiResult            (LLM 调用 + JSON 解析)
  -> PublishSnapshot     (publish_records + publish_items 冻结)
  -> PublishedArtifact   (Markdown 文件 / GitHub commit)
```

主链路在内部体现为状态机推进与跨 crate 的 use-case 编排。每段对应一个 Flow，
入口集中在 [crates/runtime/src/flows/](../../crates/runtime/src/flows/)。

各段实施细节：
- [01-feed.md](./01-feed.md) — 抓取段
- [02-extract.md](./02-extract.md) — 正文提取段
- [03-ai.md](./03-ai.md) — AI 分析段
- [04-publish.md](./04-publish.md) — 发布段

## 4. 四层架构 + 对象层

承袭 [constitution.md §3.2](../constitution.md)。任何模块必须明确所属层，跨层调用禁止。

| 层 | 承载 | 当前实现位置 |
|---|---|---|
| 1. 交互壳层 | CLI 入口、参数解析、结果呈现 | [`src/main.rs`](../../src/main.rs)、[`crates/cli/`](../../crates/cli/) |
| 2. 指令接口层 | 子命令 → 结构化意图 | [`crates/cli/src/commands/`](../../crates/cli/src/commands/) |
| 3. 流程协调层 | use-case 编排、状态推进、补偿 | [`crates/runtime/`](../../crates/runtime/) |
| 4. 能力执行层 | 网络 / 提取 / AI / 存储 / 发布 | [`crates/feed/`](../../crates/feed/)、[`crates/extractor/`](../../crates/extractor/)、[`crates/ai/`](../../crates/ai/)、[`crates/storage/`](../../crates/storage/)、[`crates/publish/`](../../crates/publish/)、[`crates/report/`](../../crates/report/) |
| 对象层 | 本体对象 / DTO / 错误 / 配置 | [`crates/domain/`](../../crates/domain/)、[`crates/config/`](../../crates/config/) |

横向能力（不属于主链路任一段，但被多段引用）：
- [`crates/observability/`](../../crates/observability/) — tracing / metrics / health / run_events

CLI 壳与 Runtime 的接缝点在 [`crates/runtime/src/context.rs`](../../crates/runtime/src/context.rs)
（`RunContext`），它是流程协调层的进入点。详见 [09-cli-and-runtime.md](./09-cli-and-runtime.md)。

## 5. 核心不变量

以下是工程宪法的直接推论。任何违反需经审批（见 [constitution.md §7.1](../constitution.md)）。

1. **去重前不抓正文** — 三层去重命中即停，节省外部请求与存储
2. **所有队列固定容量** — 内存不允许随上游漂浮
3. **入库后状态自描述** — 任何 worker 重启都能从数据库恢复，不依赖进程内对象
4. **AI 任务只从数据库领取** — `article_ai_results` pending 行是唯一信号源
5. **发布先冻结快照** — 渲染必须读 `publish_items.frozen_*` 列，绝不重新查 articles
6. **所有并发任务都 claim + lease** — 防止重复领取、防止丢任务
7. **所有外部输入能力上可回放** — `config.artifact.retention_policy` 控制是否捕获 RawArtifact
8. **核心状态与配置单一真相源** — 内存缓存 / UI 状态 / 临时文件不得与持久层并列
9. **失败路径与观测点同步设计** — 不允许后补日志、后补错误分支

不变量 7 的实施细节见 [10-replay-and-backfill.md](./10-replay-and-backfill.md)；
不变量 8 的存储层映射见 [05-storage.md](./05-storage.md)；
不变量 9 的实施见 [07-observability.md](./07-observability.md) 与 [11-error-and-recovery.md](./11-error-and-recovery.md)。

## 6. 设计驱动模式

系统采用五个"驱动"贯穿所有阶段。这些不是抽象概念，而是骨架级实现要求。

| 驱动 | 含义 | 对应实现 |
|---|---|---|
| 阶段驱动 | 行为分解为可恢复的有限状态 | 4 个状态机定义于 [`crates/domain/src/state.rs`](../../crates/domain/src/state.rs) |
| 快照驱动 | 发布、回放走冻结副本 | `publish_items` 冷列；`raw_artifacts` |
| 回放驱动 | 关键外部输入可在脱离外网时重放 | `cli replay --kind={html,ai}` |
| 租约驱动 | 任务领取通过 claim + lease | `feed_entries` / `article_ai_results` / `reindex_jobs` / `publish_records` |
| 版本化驱动 | 规则 / prompt 升级走 reindex | `rule_versions` + `reindex_jobs` |

详细每个驱动的实现位置见对应章节。

## 7. 12 crate 职责一表概览

| Crate | 层 | 一句话职责 |
|---|---|---|
| `rss-ai-news` (binary) | 1 | 二进制入口；初始化 config / tracing / storage / runtime |
| `cli` | 1+2 | clap derive 子命令；调度到 `runtime` Flow |
| `domain` | 对象 | 本体对象、状态机、DTO、纯 Rust 类型 |
| `config` | 对象 | TOML/.env 解析、schema 校验、validate-config |
| `runtime` | 3 | Flow 编排、`RunContext`、错误聚合、artifact 写入 |
| `storage` | 4 | sqlx 双方言（SQLite + PG）、repo trait、migrations |
| `feed` | 4 | HTTP client、RSS/Atom/JSON parser、conditional request |
| `extractor` | 4 | HTML 抓取、策略链（readability / 密度 / fallback） |
| `ai` | 4 | OpenAI 兼容 client、prompt 组装、JSON 解析 |
| `report` | 4 | Markdown renderer、frontmatter、rebuild-report |
| `publish` | 4 | local fs target + GitHub target、atomic batch、retry |
| `observability` | 横向 | tracing 初始化、prometheus exporter、health probe |

详细一图概览见 [../map/architecture.md](../map/architecture.md)；
crate 间依赖与公开导出见 [../map/modules.lisp](../map/modules.lisp)。

## 8. 阅读后续章节的建议顺序

- **想理解系统骨架** → 本章 + [./08-state-machines.md](./08-state-machines.md) + [./13-non-goals.md](./13-non-goals.md)
- **要改某个能力** → 直接读对应章节（01–04 主链路 / 05–07 横向）
- **要排障 / 部署** → 跳到 [../operations/](../operations/) 与 [./11-error-and-recovery.md](./11-error-and-recovery.md)
- **想知道为什么这么决定** → 翻 [../adr/](../adr/)

## 9. 当前实现入口

| 总览维度 | 入口路径 |
|---|---|
| 二进制入口 | [`src/main.rs`](../../src/main.rs) |
| CLI 子命令 | [`crates/cli/src/commands/`](../../crates/cli/src/commands/) |
| Flow 编排 | [`crates/runtime/src/flows/`](../../crates/runtime/src/flows/) |
| 本体定义 | [`crates/domain/src/`](../../crates/domain/src/) |
| 状态机 | [`crates/domain/src/state.rs`](../../crates/domain/src/state.rs) |
| 存储池 | [`crates/storage/src/lib.rs`](../../crates/storage/src/lib.rs)（`StoragePool` enum） |
| Migrations | [`migrations/sqlite/`](../../migrations/sqlite/) + [`migrations/postgres/`](../../migrations/postgres/) |
| 配置 schema | [`crates/config/src/`](../../crates/config/src/) |
| Workflow | [`.github/workflows/{ci,release}.yml`](../../.github/workflows/) |

代码路径过时时，在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移，
不在本章直接修改 — 本章是契约，按 [../README.md](../README.md) 中"文档真相源原则"小节演进。
