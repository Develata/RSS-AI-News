# RSS-AI-News 文档

本目录是 RSS-AI-News 的设计与运维文档真相源。当前版本：v0.3.0（细节优化期）。

## 项目一句话定位

单进程一次性 CLI 管线：`抓取 RSS / Atom / JSON Feed → 正文提取 → AI 摘要筛选 → Markdown 报告 → 本地或 GitHub 发布`。不内置 cron，由外部调度器触发。

## 文档导览（按角色）

### 我是新人，想理解这个项目

1. [工程宪法](./constitution.md) — 不变的最高约束（5 分钟）
2. [plan/00-overview.md](./plan/00-overview.md) — 系统总览：本体、四层架构、主链路
3. [map/architecture.md](./map/architecture.md) — 一页架构图
4. 选一个能力章节深入：[plan/01-feed.md](./plan/01-feed.md)、[plan/04-publish.md](./plan/04-publish.md) ...

### 我要做日常优化 / 修 bug / 加能力

1. [map/architecture-code.lisp](./map/architecture-code.lisp) — 找到目标符号当前在哪个 crate
2. 跳到对应 [plan/](./plan/) 章节读契约
3. 查 [acceptance-cases/](./acceptance-cases/) 看现有验收覆盖
4. 改完后按 [handoffs/TEMPLATE.md](./handoffs/TEMPLATE.md) 写一份交接

### 我要部署 / 排障

1. [operations/cli-reference.md](./operations/cli-reference.md) — 所有子命令 + flag + exit code
2. [operations/docker-and-scheduler.md](./operations/docker-and-scheduler.md) — Docker + 外挂 cron
3. [operations/postgres-deployment.md](./operations/postgres-deployment.md) — PG 切换
4. [operations/troubleshooting.md](./operations/troubleshooting.md) — doctor + 常见故障

### 我要做架构评审 / 复盘决策

1. [adr/](./adr/) — 所有架构决策记录（时间序）
2. [reports/releases/](./reports/releases/) — 每个 release 的完成态快照
3. [map/architecture-diff.md](./map/architecture-diff.md) — plan vs code 的漂移注册

## 目录结构

```
docs/
├── constitution.md             工程宪法（最高约束，不变）
├── README.md                   本文件
├── AGENTS.md                   agent 工作指南
│
├── plan/                       实施详解：怎么设计、哪里做、功能怎么实现
├── acceptance-cases/           功能清单 + 验收状态（pipelines + commands 两层）
├── map/                        现状导航地图（lisp 双视图，与 codegraph 联动）
├── operations/                 运维 / 部署 / 排障
├── adr/                        架构决策记录（时间序）
├── reports/                    时间戳快照（release / audit）
└── handoffs/                   append-only 工作日志
```

各子目录的角色与文件清单见各自的 `README.md`。

## 文档真相源原则

1. **`constitution.md`** 是最高约束，所有其他文档不得与之冲突
2. **`plan/`** 是实施真相源：系统当前如何设计与实现的权威说明
3. **`acceptance-cases/`** 是功能验收真相源：每个能力的覆盖状态与测试入口
4. **`adr/`** 是决策真相源：为什么这样做、何时做的、有什么后果
5. **`map/`** 是导航索引，不承载契约内容；它指向 `plan/` 与代码
6. **`operations/`** 是运维真相源：跑起来 / 排障 / 部署的权威步骤
7. **`reports/` 与 `handoffs/`** 是时间序事实记录，不改写 1–6 的真相源

当文档与代码不一致时：
- 修代码以匹配契约（plan + adr） → 默认路径
- 或修契约以承认现实（更新 plan 或新增 adr） → 必须经过审批

## 与旧文档的关系

旧 `docs/` 已归档至 `docs-backup/`（git mv 保留历史）。新结构落地稳定后，`docs-backup/` 将被删除。
迁移映射见 [迁移计划](../docs-backup/) 与当前文档结构对照。
