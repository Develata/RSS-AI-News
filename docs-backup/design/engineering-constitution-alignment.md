# 设计文档与工程宪法的对齐修正

## 1. 目标

本文档不是替代《工程宪法》或《核心设计哲学》，而是定义二者如何落到本仓库的具体骨架级产物。

它回答的唯一问题：按工程宪法回看后，新 Rust 蓝图必须把哪些条目上提为硬约束，而不是可选项。

与宪法、哲学文档已覆盖的内容不再复述；若条目与 [工程宪法](../constitution.md) 或 [核心设计哲学](./design-philosophy.md) 发生表述冲突，以宪法为准。

## 2. 必须上提为硬约束的 8 项

按工程宪法的优先级排序与本体优先规则，下列 8 项必须从首版骨架开始存在。任何实现方案若缺其中一项，都不得进入主干：

1. **固定设计顺序** — 骨架 → 模块 → 实现的单向推进必须反映到文档层次与任务拆解中。
2. **四层架构 + 对象层** — 任何新能力必须明确归属哪一层，禁止跨层混写。
3. **单一真相源** — 订阅源、feed 条目、文章、AI 结果、发布、配置各有唯一持久化真相源；内存缓存、进程快照、临时文件不得与之并列。
4. **幂等 + 租约 + 时序一致性** — 所有可能并发或重入的流程必须具备 `claim + lease + attempt_count + conflict resolution` 的完整设计。
5. **失败 → 观测 → 验证内建** — 每条核心流程必须在设计阶段回答六问：成功路径、失败条件、错误传播、重试边界、用户可见结果、观测与验证方式。
6. **版本责任前置** — 配置 schema、DTO 协议、提取规则、AI prompt、AI 输出协议、DB migration、发布渲染版本均负有版本号，不接受"以后再加"。
7. **退出路径正式定义** — `FeedSource` / `FeedEntry` / `RawArtifact` / `PublishSnapshot` / 本地报告的停用、归档、删除路径必须与创建路径同时定义。
8. **replay / backfill / rebuild-report / reindex 作为正式能力** — 这四项不是调试脚本，而是 CLI 主命令，位于流程协调层。

## 3. 对既有蓝图应新增的能力

### 3.1 `doctor`

作为主 CLI 命令存在，覆盖配置、数据库、AI endpoint、GitHub token、RSSHub base URL、时区的健康检查。不作为辅助脚本。

### 3.2 `replay`

必须能重放 `feed payload`、`html payload`、`ai raw response` 三类 artifact，脱离外网可执行。

### 3.3 `backfill`

必须能对历史 `Article` / `ArticleAiResult` 补跑正文提取与 AI 处理，并携带版本信息。

### 3.4 `rebuild-report`

必须能从 `PublishSnapshot` 重建 Markdown，且不触发新一轮 AI 调用。

### 3.5 `reindex`

当 link 规范化规则、content hash 规则、派生字段或分类映射发生变更时，系统必须有重算通道。

## 4. 对 workspace 设计的修正要求

crate 划分必须能映射回四层骨架：

- `app` / `cli` 承载交互壳层
- `runtime` 承载指令接口层 + 流程协调层
- `feed` / `extractor` / `ai` / `storage` / `publish` / `report` 承载能力执行层
- `domain` 承载对象层与稳定业务模型
- `config` / `observability` 作为横切能力 crate

`domain` crate 必须是稳定层：外部脏数据只能在 adapter 层被清洗，进入 `domain` 后必须成为稳定对象。`domain` 不得依赖任何具体 I/O crate。

## 5. 对数据库设计的修正要求

### 5.1 发布快照是硬约束

`publish_records` + `publish_items` 必须存在；发布不得以"实时查库 + 直接推 GitHub"的方式进行。字段级细节见 [storage-schema](./storage-schema.md)。

### 5.2 raw_artifact 必须可配置保留

至少支持：全关、仅失败保留、采样保留、调试模式全保留。不接受"默认永远保留"或"默认永远不保留"的极端策略。

### 5.3 数据库 schema 负版本责任

migration 号、配置版本、规则版本、prompt 版本、output schema 版本必须都可关联到具体的运行时结果行。

## 6. 对交付架构的修正要求

### 6.1 根基兼容性优先于极限轻量

交付层必须保障 CA 证书、tzdata、TLS/HTTP 行为、GitHub API 兼容运行。默认不追 `scratch` 镜像。

### 6.2 生产镜像 + 调试镜像双产物是合理的

符合"根基兼容性 > 性能/内存/外存"的优先级。

## 7. 判定

任何实现方案在进入主干前必须能同时回答：

- 是否服从固定设计顺序
- 是否落在四层架构 + 对象层之内
- 是否服从单一真相源
- 是否具备幂等、租约、时序一致性
- 是否定义了失败、观测、验证三件事
- 是否对变更结构承担版本责任
- 是否定义了退出路径
- 是否在 replay / backfill / rebuild-report / reindex 四项能力内有对应位置

若任何一项答案不成立，该方案不得进入主干。
