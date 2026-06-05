# AC-C-08: scheduler 镜像与外挂调度

## 功能描述

`rss-ai-news` 二进制本身**不**内置定时器（详见 [../../plan/13-non-goals.md](../../plan/13-non-goals.md)）。
"scheduler" 是一个独立**镜像**：在 runtime 镜像基础上加 supercronic + entrypoint，
通过 cron 表达式周期触发底层 single-shot CLI。

两种调度形态（优先级从高到低）：
1. **外挂 crontab 文件**（`RSS_CRONTAB_FILE`，默认 `/app/crontab`）：多行表达式 + 完整命令
2. **单行 env 模式**（`RSS_CRON_SCHEDULE` + `RSS_CRON_COMMAND`）：简单场景

面向场景：1panel 风格容器面板部署、需要常驻 + cron 触发但又不想引入宿主 cron。

## 验收标准

### 命中条件（success path）

- scheduler 镜像 release 时与 runtime 镜像同 tag 同时构建发布
- entrypoint 在 `RSS_CRONTAB_FILE` 存在且非空时使用文件模式，跳过 env 模式
- 文件模式：把挂载的 crontab 完整交给 supercronic
- env 模式：内存拼一行 crontab 给 supercronic
- cron job 的 stdout/stderr 透传到容器 stdout，`docker logs` 可见
- 首次 migrate **必须**通过一次性 runtime 镜像执行，scheduler 容器**不**自动 migrate

### 失败条件（failure path）

- `RSS_CRONTAB_FILE` 文件不存在或为空 + env 也未设 → entrypoint 退出非 0
- supercronic 启动失败 → 容器立即 Exit
- 触发的 `rss-ai-news <cmd>` 返回非 0 → cron job log 可见，supercronic 继续运行（不连锁退出）
- 触发命令缺 binary 路径 / 缺 `--config-dir` → cron 内对应行失败但不影响其它 cron 行

### 镜像层验收

- `docker/Dockerfile` 中 scheduler stage `FROM runtime`，无源代码重编译
- supercronic 静态二进制位于 `/usr/local/bin/supercronic`
- ENTRYPOINT 指向 `/usr/local/bin/scheduler-entrypoint.sh`
- CI release workflow 一次推送同 tag 的两套镜像到 GHCR

## 测试覆盖

scheduler 镜像与 entrypoint 的验收**没有**专门的 Rust 测试；验收通过以下方式保证：

| 验收项 | 验证方式 |
|---|---|
| 镜像构建可重复 | `.github/workflows/ci.yml` 的 `docker-build` job |
| 双镜像发布 | `.github/workflows/release.yml`（push tag 触发） |
| entrypoint 调度形态 | 手动 smoke：`docker run -e RSS_CRON_SCHEDULE=... -e RSS_CRON_COMMAND=...` 观察 supercronic 输出 |
| 文件模式优先 | 同上：挂载 `/app/crontab` 后 `docker logs` 中应出现 "using mounted crontab" |
| 触发命令 single-shot 语义 | 由 runtime 镜像的全套 Rust 测试间接覆盖（每次 cron 触发即一次完整 run） |

## 当前状态

`partial`

已知缺口：
- entrypoint 与 compose 没有自动化 e2e；当前依靠 release 流程 + 手动 smoke 验收
- 没有 `--check-only` 子命令用于在 CI 里 lint crontab 表达式（已计入潜在改进）

## 相关文档

- 设计：[../../plan/12-deployment.md](../../plan/12-deployment.md) §4 scheduler 入口
- 边界约束：[../../plan/13-non-goals.md](../../plan/13-non-goals.md) §不内置 cron
- 入口脚本：[`docker/scheduler-entrypoint.sh`](../../../docker/scheduler-entrypoint.sh)
- compose：[`docker/docker-compose.scheduler.yml`](../../../docker/docker-compose.scheduler.yml)
- 决策：`../../adr/0001-single-shot-cli-no-builtin-cron.md`
