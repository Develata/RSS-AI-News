# operations/ — 运维 / 部署 / 排障

本目录回答："如何把 RSS-AI-News **跑起来**、**部署上去**、**出问题时怎么定位**？"

[../plan/](../plan/) 讲设计，[../acceptance-cases/](../acceptance-cases/) 讲验收，这里讲**执行**。

## 文件清单

| 文件 | 内容 |
|---|---|
| [cli-reference.md](./cli-reference.md) | 所有子命令 + flag + exit code 速查表 |
| [local-dev.md](./local-dev.md) | 本地开发：cargo / fmt / clippy / pre-commit hook |
| [docker-and-scheduler.md](./docker-and-scheduler.md) | multi-stage 镜像 + scheduler 容器 + 外挂 cron |
| [postgres-deployment.md](./postgres-deployment.md) | driver=postgres 切换 + 端到端 PG 部署 |
| [ci-and-release.md](./ci-and-release.md) | GitHub Actions workflow + GHCR + 版本流程 |
| [troubleshooting.md](./troubleshooting.md) | doctor 用法 + 常见故障定位 |

## 与其它目录的关系

- **[../plan/12-deployment.md](../plan/12-deployment.md)** 是部署架构的设计；本目录是设计的运维侧落地
- **[../adr/](../adr/)** 中所有与运维相关的决策（如 single-shot CLI、PG 走实补）在这里有运维侧映射
- 关键运维事件（线上故障 / 升级 / 切换）写入 [../handoffs/](../handoffs/)
