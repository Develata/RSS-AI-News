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
- entrypoint 在 `RSS_CRONTAB_FILE` 是非空普通文件时使用文件模式，跳过 env 模式；不存在或为空时走 env 模式
- 文件模式：把挂载的 crontab 完整交给 supercronic
- env 模式：拼一行 `<RSS_CRON_SCHEDULE> /usr/local/bin/rss-ai-news --config-dir /app/configs <RSS_CRON_COMMAND>`
  给 supercronic；supercronic 以 `/bin/sh -c` 执行命令部分，`RSS_CRON_COMMAND` 只经一次 shell 解析，
  带引号的含空格参数（如 `--output 'daily report.md'`）保持为一个 argv 元素
- cron job 的 stdout/stderr 透传到容器 stdout，`docker logs` 可见
- 首次 migrate **必须**通过一次性 runtime 镜像执行，scheduler 容器**不**自动 migrate

### 失败条件（failure path）

- `RSS_CRONTAB_FILE` 文件不存在或为空 + env 也未设 → entrypoint 退出非 0
- `RSS_CRONTAB_FILE` 存在但不是普通文件（例如 bind-mount 的宿主文件不存在时 Docker 自动建出的目录）→
  entrypoint 报错退出 1，**不**静默降级到 env 模式（镜像自带默认 `RSS_CRON_*`，降级会悄悄换成默认调度）
- `RSS_CRON_SCHEDULE` / `RSS_CRON_COMMAND` 含换行或回车 → entrypoint 退出 1（否则会拼出额外的 crontab 行）
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
| env 模式真实执行一个 job、含空格参数不被拆分 | `ci.yml` `docker-build` 的 `smoke scheduler runs a job` 步骤与 docker lane 的 `docker-scheduler-job-contract`：每 2 s 一次的 7 段表达式，命令 `validate-config 'spaced arg'`（clap 在读配置前即拒绝，无副作用），断言 `docker logs` 含 `unexpected argument 'spaced arg'`，最多等 75 s |
| 目录型 crontab 拒绝、换行拒绝、空文件降级 | 手动 shell 级测试（在 `debian:bookworm-slim` 的 dash 下用 stub supercronic 运行 entrypoint）；无 CI 自动化 |
| 文件模式优先 | 同上：挂载 `/app/crontab` 后 `docker logs` 中应出现 "using mounted crontab" |
| 触发命令 single-shot 语义 | 由 runtime 镜像的全套 Rust 测试间接覆盖（每次 cron 触发即一次完整 run） |

## 当前状态

`partial`

已知缺口：
- env 模式已有 CI e2e（真实执行一个 job）；文件模式、目录拒绝与换行拒绝仍只有手动验证，compose 没有自动化 e2e
- 没有 `--check-only` 子命令用于在 CI 里 lint crontab 表达式（已计入潜在改进）

## 相关文档

- 设计：[../../plan/12-deployment.md](../../plan/12-deployment.md) §4 scheduler 入口
- 边界约束：[../../plan/13-non-goals.md](../../plan/13-non-goals.md) §不内置 cron
- 入口脚本：[`docker/scheduler-entrypoint.sh`](../../../docker/scheduler-entrypoint.sh)
- compose：[`docker/docker-compose.scheduler.yml`](../../../docker/docker-compose.scheduler.yml)
- 决策：`../../adr/0001-single-shot-cli-no-builtin-cron.md`
