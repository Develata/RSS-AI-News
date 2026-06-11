# CLI 速查表

`rss-ai-news <global-flags> <subcommand> <subcommand-flags>`。
完整 CLI surface 由 [`crates/cli/src/args.rs`](../../crates/cli/src/args.rs) 用 clap derive 维护；
本文件是按子命令分组的人类可读速查。

## 全局 flag（任意子命令前都可加）

| flag | 默认 | 作用 |
|---|---|---|
| `-c, --config-dir <PATH>` | `configs` | `app.toml` + `categories/*.toml` 所在目录 |
| `--db-path <PATH>` | `app.toml [database].sqlite_path` | 覆盖 SQLite DB 路径（PG 用 `DATABASE_URL` env） |
| `--log-level <STR>` | `info` | tracing 过滤器（`RUST_LOG` 可二次覆盖） |
| `--log-format <pretty\|json>` | `pretty` | tracing 输出格式 |
| `--log-file <PATH>` | `""` | 非空 → 同时写 stderr + 日轮转文件 |
| `--metrics-bind <HOST:PORT>` | `""` | 非空 → 启动 Prometheus `/metrics` HTTP 端点 |
| `-o, --output-format <pretty\|json>` | `pretty` | 子命令 summary 输出格式 |
| `-n, --dry-run` | `false` | 总开关；部分子命令额外暴露 `--dry-run` |
| `-C, --category <KEY>` | `None` | 仅处理该分类（多分类场景隔离） |
| `--timezone <IANA>` | `app.toml [publish].target_timezone` | 覆盖发布时区 |

> **`--max-batches`** 不是全局 flag。仅 `ingest` / `ai-run` / `run` 三个子命令本地暴露
> （F7-1 修复，详见 args.rs 注释）。

## 子命令一览（12 个）

| 子命令 | 一句话 | 详见 |
|---|---|---|
| `ingest [flags]` | Feed 抓取 + 解析 + 三层去重 | [../acceptance-cases/pipelines/01-feed-ingest.md](../acceptance-cases/pipelines/01-feed-ingest.md) |
| `ai-run [flags]` | 生成 + 处理 AI 任务 | [../acceptance-cases/pipelines/03-ai-analysis.md](../acceptance-cases/pipelines/03-ai-analysis.md) |
| `publish [flags]` | 单分类发布（5 阶段） | [../acceptance-cases/pipelines/04-publish-local-and-github.md](../acceptance-cases/pipelines/04-publish-local-and-github.md) |
| `publish-all [flags]` | 全分类发布 | 同上 |
| `run [flags]` | ingest + ai-run + publish-all 一次跑完 | 同各能力章 |
| `doctor [--deep]` | 健康检查（含 deep 跨表不变量） | [../acceptance-cases/commands/doctor.md](../acceptance-cases/commands/doctor.md) |
| `validate-config` | 三阶段配置校验 | [../acceptance-cases/commands/validate-config.md](../acceptance-cases/commands/validate-config.md) |
| `migrate {run\|check}` | DB schema 迁移 | [../acceptance-cases/commands/migrate.md](../acceptance-cases/commands/migrate.md) |
| `replay --kind=<K> {--id\|--key} <V>` | 只读重做（feed/html/ai） | [../acceptance-cases/commands/replay.md](../acceptance-cases/commands/replay.md) |
| `backfill --target=<T> [flags]` | 重做窗内业务（extract / ai） | [../acceptance-cases/commands/backfill.md](../acceptance-cases/commands/backfill.md) |
| `reindex {--target=<T>\|--abort=<JOB_ID>} [--dry-run]` | 版本化规则升级 | [../acceptance-cases/commands/reindex.md](../acceptance-cases/commands/reindex.md) |
| `rebuild-report {--publish-id <ID>\|--date <D>}` | 按 snapshot 重新渲染历史报告 | [../acceptance-cases/commands/rebuild-report.md](../acceptance-cases/commands/rebuild-report.md) |

> 注：没有独立的 `extract` 子命令 —— 抓正文与抓 feed 合并在 `ingest` / `run` 中；离线重做走
> `replay --kind=html`；批量重做走 `backfill --target=extract`。

## 关键子命令 flag

### `ingest`
- `--source <KEY>`：只跑指定 source
- `--skip-fetch`：跳过 HTTP，仅做去重 / 入库（调试）
- `--batch-size <N>`（默认 `50`）
- `--max-batches <N>`：覆盖 `runtime.max_batches_per_run`；`0` = 不限

### `ai-run`
- `--batch-size <N>`（默认 `20`）
- `--model <ID>`：覆盖 `[ai].model`
- `--max-batches <N>`

### `publish` / `publish-all`
- `--date <YYYY-MM-DD>`：指定 report 日期
- `--local-only`：跳过远端 publish
- `--force`：强制重做（即使已有 publish_record）

### `doctor`
- `--deep`：启用跨表不变量扫描（I1–I6 / I8 过期 running lease / I9 预算耗尽的可领取行）

### `replay`
- `--kind <feed\|html\|ai>`：必填
- `--key <KEY>` **或** `--id <I64>`：互斥
- `--diff`：与已有结果对比

### `backfill`
- `--target <extract\|ai>`：必填
- `--date-from <YYYY-MM-DD>` / `--date-to <YYYY-MM-DD>`
- `--batch-size <N>`（默认 `50`）
- 仅 `--target ai`：`--prompt-version-tag` / `--prompt-version-description` / `--model`

### `reindex`
- `--target <link_hash\|content_hash\|categories\|all>`（与 `--abort` 互斥）
- `--abort <JOB_ID>`：取消指定 reindex_jobs 行
- `--batch-size <N>`（默认 `100`）
- `--dry-run`

### `rebuild-report`
- `--publish-id <I64>` **或** `--date <YYYY-MM-DD>`：互斥
- `--output <PATH>`：自定义输出路径

### `migrate`
- 子动作：`migrate run` / `migrate check`

### `run`
- `--ingest-batch-size <N>` / `--ai-batch-size <N>`
- `--publish-date <YYYY-MM-DD>`
- `--max-batches <N>`

## Exit Code

退出码由 [`crates/cli/src/exit_code.rs`](../../crates/cli/src/exit_code.rs) 的 `ExitCode` enum 决定。
**当前只有 4 个值**：

| Code | 变体 | 含义 |
|---:|---|---|
| `0` | `Success` | 全量成功；也含部分非致命跳过（如 `SnapshotEmpty`） |
| `1` | `RuntimeError` | 业务 / 运行时错误（`RuntimeError::*` 透传，含 `DoctorFailed` / `ReplayArtifactNotFound` / `PublishRecordNotFound` / `MigrateCheckPending` 等） |
| `2` | `UserError` | CLI 参数错（clap 解析失败 / 非法 flag 组合 / `ReindexTargetRequired` 等） |
| `78` | `ConfigError` | 配置错（schema / env 缺失 / 模板非法 / `AiRunWhileDisabled`）—— sysexits `EX_CONFIG` |

详尽分类与 sysexits 收敛历史见 [../plan/11-error-and-recovery.md](../plan/11-error-and-recovery.md) §5。

## 相关文档

- 设计层：[../plan/09-cli-and-runtime.md](../plan/09-cli-and-runtime.md)
- 错误模型：[../plan/11-error-and-recovery.md](../plan/11-error-and-recovery.md)
- 子命令验收：[../acceptance-cases/commands/](../acceptance-cases/commands/)
- args 实现：[`crates/cli/src/args.rs`](../../crates/cli/src/args.rs)
