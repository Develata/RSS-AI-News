# ADR 0001: 单次执行 CLI，不内置 cron

- 日期：2026-02（建造期 W0 期间确立，本 ADR 是事后留痕）
- 状态：`accepted`
- 决策者：项目主作者

## Context

RSS-AI-News 的核心需求是周期性地抓取 → 提炼 → 发布。一个直观的实现是把整条
流水线塞进一个常驻进程，自己维护内部定时器（如 tokio interval）逐段触发。
但常驻+内置 cron 形态有几个长期成本：

1. **进程内状态污染**：常驻进程的逻辑状态（重试计数、租约、并发槽）必须与
   持久层保持一致；任何重启 / 崩溃都要重做状态恢复设计。
2. **失败放大**：单点常驻进程崩溃时所有调度同时失效；外挂调度则每次 run
   独立可观测可重启。
3. **运维耦合**：cron 调整需要重启进程；调度参数与代码强绑。
4. **可测性下降**：调度循环混在业务流程里，单元测试要 mock 时间。

候选方案：
- (a) 常驻进程 + 内置 tokio interval / cron 库
- (b) **single-shot CLI + 外挂调度**（系统 cron / supercronic / k8s CronJob 等）
- (c) Actor 系统（Actix / etc.）+ 内部调度器

## Decision

采用 **(b) single-shot CLI**：每次 `rss-ai-news <subcommand>` 运行完一段流水线
（或 `run` 全段）后退出，把"什么时候触发"完全外化给宿主调度器。

- 二进制本身**不内置**任何定时循环、不监听信号、不长驻
- 提供 `scheduler` 镜像作为兜底：runtime 镜像 + supercronic + entrypoint，
  但 supercronic 也是外挂逻辑，不是 Rust 进程内的事

## Consequences

### 正面后果

- 每次 run 是独立的、可重复观察的事件：日志 / metrics / run_events 都按 run_id 切片
- 崩溃只影响当前 run；下一轮 cron 触发即自动恢复
- 调度策略可在不重新部署镜像的情况下调整（改 crontab 即可）
- 测试只需测 Flow，不必 mock 时间或 interval

### 负面后果 / 代价

- 用户必须自己配 cron（宿主 cron / docker compose + scheduler / k8s）
- 每次 run 都要重新初始化 config / pool / tracing，启动有固定开销（~100ms 级）
- 同步原语（任务领取的 claim+lease）必须在数据库层做，不能借助进程内 mutex —— 这反而强制
  了 [[ADR-0002]] 的 stage-driven-lease-claim 模型

### 后续行动

- `scheduler` 镜像作为部署便利（`docker/Dockerfile` 的 scheduler stage + `docker/scheduler-entrypoint.sh`）
- 在 `plan/13-non-goals.md` 与 `plan/12-deployment.md` §4 显式声明"不内置 cron"

## Links

- 设计：[../plan/13-non-goals.md](../plan/13-non-goals.md)
- 部署：[../plan/12-deployment.md](../plan/12-deployment.md)
- 验收：[../acceptance-cases/commands/scheduler.md](../acceptance-cases/commands/scheduler.md)
- 相关 ADR：[[ADR-0002]]（stage-driven lease+claim 由本决策派生）
