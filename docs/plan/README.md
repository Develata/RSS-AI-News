# plan/ — 项目实施详解

本目录回答："这个项目**当前**是怎么设计的、哪里做了什么、功能怎么实现的？"

按**能力 / 流水线段**组织，每章自包含。这不是"应当如此的蓝图"（旧建造期范式），而是"系统当前真实长什么样"的权威说明。

## 章节列表

| # | 章节 | 内容 |
|---|---|---|
| 00 | [overview.md](./00-overview.md) | 系统总览：本体对象 / 四层 + 对象层 / 主链路 |
| 01 | [feed.md](./01-feed.md) | Feed 抓取段：source / 条件请求 / parser / ingest 闭环 |
| 02 | [extract.md](./02-extract.md) | 正文提取段：策略链 / 三层去重 / replay-html |
| 03 | [ai.md](./03-ai.md) | AI 分析段：client / prompt / versioning / ai-run 闭环 |
| 04 | [publish.md](./04-publish.md) | 发布段：snapshot / renderer / local + GitHub / per-category path / atomic batch |
| 05 | [storage.md](./05-storage.md) | 存储：schema / claim-and-lease / 多方言 / migration / reindex |
| 06 | [config.md](./06-config.md) | 配置：schema / .env / 占位符 / validate-config |
| 07 | [observability.md](./07-observability.md) | tracing / metrics / run-events / health |
| 08 | [state-machines.md](./08-state-machines.md) | 4 状态机集中说明（跨能力共用） |
| 09 | [cli-and-runtime.md](./09-cli-and-runtime.md) | CLI surface + Runtime context + 主入口 |
| 10 | [replay-and-backfill.md](./10-replay-and-backfill.md) | 跨能力的回放 / 补齐机制 |
| 11 | [error-and-recovery.md](./11-error-and-recovery.md) | 错误模型 / exit code / 失败路径 / 重试边界 |
| 12 | [deployment.md](./12-deployment.md) | Docker multi-stage / scheduler / GHCR / CI / PG 部署 |
| 13 | [non-goals.md](./13-non-goals.md) | 明确不做的事（继承自旧蓝图 §14 不变量） |

## 与其它目录的关系

- **[../constitution.md](../constitution.md)**：plan 不得与之冲突
- **[../acceptance-cases/](../acceptance-cases/)**：每个能力章节对应若干 case，相互引用
- **[../map/](../map/)**：从 plan 章节抽取的导航索引
- **[../adr/](../adr/)**：plan 内的非显然决策都应在 adr 中留痕
- **[../operations/](../operations/)**：plan 讲设计，operations 讲怎么跑

## 章节末尾的"当前实现"段落

每章末尾必须列出至少 3 个真实代码路径，作为契约 ↔ 实现的对照。这是 plan/ 跟代码同步的硬约束。
路径过时时，由首先发现的人在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。
