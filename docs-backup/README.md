# RSS-AI-News 文档总览

本目录采用“宪法 -> 设计哲学 -> 工程蓝图 -> 任务分解”的结构，参考 Spec Kit 的规范驱动工作流，但以本仓库自己的工程宪法为最高约束。

## 目录

- `constitution.md`
  - 项目级工程宪法。定义不可轻易突破的总原则、优先级、骨架约束与治理规则。
- `design/`
  - 核心设计哲学、宪法落地对齐、Python 历史教训，以及实现级契约（存储 schema、状态机、配置、DTO、CLI、错误与可观测性、回放与 artifact、依赖选型）。
- `plan/`
  - 可直接施工的工程蓝图。回答“系统具体怎么建”。
- `task/`
  - 从蓝图拆解出的可执行任务。回答“按什么顺序做、做到哪些模块”。
- `handoffs/`
  - **append-only 工作日志**。按日期滚动，记录"实际发生了什么"。不替代 `design/` 真相源，也不改写蓝图 / 任务分解；两者由各自的正式文档承担更新。详见 [handoffs/README.md](./handoffs/README.md)。

## 阅读顺序

1. [工程宪法](./constitution.md)
2. [核心设计哲学](./design/design-philosophy.md)
3. [宪法落地对齐](./design/engineering-constitution-alignment.md)
4. [Python 历史教训](./design/python-legacy-lessons.md)
5. [存储 schema](./design/storage-schema.md)
6. [状态机](./design/state-machine.md)
7. [配置 schema](./design/config-schema.md)
8. [内部 DTO 契约](./design/internal-dto-contracts.md)
9. [CLI 语义](./design/cli-semantics.md)
10. [错误模型与可观测性](./design/error-and-observability.md)
11. [回放与 Artifact](./design/replay-and-artifacts.md)
12. [依赖选型](./design/dependency-choices.md)
13. [工程蓝图](./plan/full-rust-rss-ai-news-blueprint.md)
14. [任务拆解](./task/full-rust-rss-ai-news-blueprint-tasks.md)

> 条目 4-12 是"实现级契约 → 施工蓝图"的关系，与 [蓝图 §15](./plan/full-rust-rss-ai-news-blueprint.md) 的 Phase 0–5 / [任务文档](./task/full-rust-rss-ai-news-blueprint-tasks.md) 的 Workstream W0–W10 配合阅读，缺一不可。

## 方法

本仓库采用规范驱动开发，流程为：

1. 宪法先行
2. 骨架后定
3. 模块再列
4. 实现最后

所有后续实现、评审、重构、迁移与功能扩展都应回到这套文档进行对照。
