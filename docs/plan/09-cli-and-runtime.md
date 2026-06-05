# 09 — CLI surface + Runtime context

本章说明：
1. CLI 子命令的整体形态与全局 flag 约定
2. Runtime 层的 `RunContext` —— 流程协调层的接缝点
3. CLI 壳到 Runtime Flow 的调度路径

实际可执行的 CLI 用法 / 排障细节在 [../operations/cli-reference.md](../operations/cli-reference.md)
。本章只讲结构 + 契约。

## 1. CLI 命令树

```text
rss-ai-news [全局 flag] <subcommand> [子命令 flag]
├── ingest          主链路：拉取 feed → 去重 → 抓正文 → 存库
├── ai-run          领取待 AI 处理的文章，调用 LLM，写回结果
├── publish         选稿 → 冻结快照 → 渲染 → 本地落盘 → 远端推送
├── doctor          健康检查（config / db / AI / GitHub / RSSHub / tz）
├── replay          从 raw_artifact 重放解析（feed / html / ai）
├── backfill        对历史数据重跑（extract / ai）
├── rebuild-report  从冻结快照重新渲染 Markdown
├── reindex         重算派生数据（link_hash / content_hash / categories）
├── migrate
│   ├── run         执行 pending migrations
│   └── check       校验 migration 状态
├── validate-config 校验配置 schema + effective 真值表
└── run             一体化执行：ingest + ai-run + publish 顺序跑
```

入口路径：
- 二进制：[`src/main.rs`](../../src/main.rs)（`#[tokio::main] async fn main()`）
- 命令路由：[`crates/cli/src/lib.rs`](../../crates/cli/src/lib.rs)（`pub async fn run() -> ExitCode`）
- 各子命令：[`crates/cli/src/commands/`](../../crates/cli/src/commands/)（11 个 `.rs` 文件，一命令一文件）

CLI 框架：`clap` derive。每个子命令是独立 enum 变体。

## 2. 全局 Flag

可在任何子命令前使用：

| Flag | 类型 | 默认 | 用途 |
|---|---|---|---|
| `--config-dir` / `-c` | path | `./configs` | 配置目录 |
| `--db-path` | path | `app.toml` 中的值 | 覆盖 DB 路径 |
| `--log-level` | enum | `info` | tracing 级别 |
| `--log-format` | enum | `pretty` | tracing 输出格式（pretty/json） |
| `--output-format` / `-o` | enum | `pretty` | 命令结果输出格式 |
| `--dry-run` / `-n` | bool | false | 只规划不执行（v0.1.0 仅 `reindex` 实装）|
| `--category` / `-C` | string | 全部 | 只处理某分类 |
| `--timezone` | string | `app.toml` 中的值 | 覆盖时区 |

`--log-format` vs `--output-format`：
- `--log-format` 控 tracing 事件流，写 stderr
- `--output-format` 控命令最终结果，写 stdout
- JSON 模式下两者互不干扰

## 3. 子命令到 Flow 的调度路径

每个子命令的责任：
1. 解析参数（clap derive）
2. 加载配置（`config::load_all` 或 `config::load_skip_env_checks`）
3. 构造 `RunContext`
4. 调用 `crates/runtime/src/flows/<flow>.rs` 的对应 Flow
5. 把 Flow 结果转 exit code / 输出格式

调度表：

| 子命令 | 入口 | 主 Flow | 备注 |
|---|---|---|---|
| ingest | `commands/ingest.rs` | `flows::ingest::IngestFlow` | 含 feed + extract 两阶段 |
| ai-run | `commands/ai_run.rs` | `flows::ai_run::AiRunFlow` | category-scoped (v0.3) |
| publish | `commands/publish.rs` + `publish_all.rs` | `flows::publish::PublishFlow` | publish_all = atomic batch |
| doctor | `commands/doctor.rs` | `runtime/src/doctor/` | 6 个 check |
| replay | `commands/replay.rs` | `flows::ingest::extract` + `flows::ai_run` | --kind={html,ai} |
| backfill | `commands/backfill.rs` | `flows::backfill::BackfillFlow` | --target={extract,ai} |
| rebuild-report | `commands/rebuild_report.rs` | `flows::rebuild_report::RebuildReportFlow` | byte-equal 重渲染 |
| reindex | `commands/reindex.rs` | `flows::reindex::ReindexFlow` | 3 类 target |
| migrate | `commands/migrate.rs` | `storage::migrate::run/check` | 不构造 RunContext |
| validate-config | `commands/validate_config.rs` | `config::validate` | 不构造 RunContext |
| run | `commands/run.rs` | 串行调用 ingest + ai-run + publish | 一体化模式 |

`migrate` 与 `validate-config` 是**纯 config / storage 层**的命令，不进入流程协调层。
这是宪法 §3.3 壳核分离的体现：它们不需要 RunContext，所以不构造。

## 4. RunContext —— 流程协调层接缝点

`RunContext` 是 CLI 壳调用 Flow 的唯一参数。它持有：

```rust
pub struct RunContext {
    pub run_id: String,              // ULID, 用于跨进程追踪
    pub started_at: OffsetDateTime,
    pub stage: String,                // "ingest" / "ai_run" / "publish" / ...
    pub app: Arc<AppConfig>,

    // 4 个能力执行层 client
    pub feed_fetcher: Arc<dyn FeedFetcher>,
    pub html_fetcher: Arc<dyn HtmlFetcher>,
    pub strategies: Vec<Arc<dyn ContentStrategy>>,
    pub ai_client: Arc<dyn AiClient>,
    pub publish_target_local: Arc<dyn PublishTarget>,
    pub publish_target_remote: Option<Arc<dyn PublishTarget>>,

    // 10 个 Repository trait（覆盖所有持久化对象）
    pub feed_source_repo: Arc<dyn FeedSourceRepository>,
    pub feed_entry_repo: Arc<dyn FeedEntryRepository>,
    pub article_repo: Arc<dyn ArticleRepository>,
    pub ai_result_repo: Arc<dyn ArticleAiResultRepository>,
    pub publish_record_repo: Arc<dyn PublishRecordRepository>,
    pub publish_item_repo: Arc<dyn PublishItemRepository>,
    pub artifact_repo: Arc<dyn RawArtifactRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
    pub rule_version_repo: Arc<dyn RuleVersionRepository>,
    pub reindex_job_repo: Arc<dyn ReindexJobRepository>,
}
```

构造模式：CLI 子命令构造 `RunContextDeps`（同形态的所有字段），传 `RunContext::new_for_stage(stage, app, deps)`。
ULID 自动生成。

### 4.1 为什么所有 client / repo 都在 Context 里

避免 Flow 入口需要 13 个参数的"参数车祸"。所有依赖通过 `Arc<dyn Trait>` 装箱，Flow
按需借用。代价：测试时需要为不用的字段注入 dummy 值（见 `context.rs` 中 `html_fetcher`
字段紧邻的 doc 注释，约定 ingest-only flow 也必须填占位值）。

### 4.2 stage 字段的作用

`stage` 是 tracing span 的 root field。所有日志、metric、run_event 自动带上 `stage` 标签。
方便从 run_events 表反查"某 stage 的全部事件"。

## 5. exit code 速查

详细 exit code 语义见 [./11-error-and-recovery.md](./11-error-and-recovery.md)。这里只列总览：

| Exit code | 含义 |
|---|---|
| 0 | 成功（含部分非致命跳过） |
| 1 | 通用业务失败 |
| 64 | CLI 参数错（clap 自动） |
| 65 | 数据 / 协议错（如 schema drift） |
| 74 | I/O 错（DB / 文件系统） |
| 78 | 配置错（schema / 必填缺失 / 非法值） |

`migrate` / `validate-config` 在配置错误时返 78，而非 1，便于 CI / Docker scheduler 区分。

## 6. CLI 壳的不可越界

宪法 §3.3 壳核分离的硬约束：

- CLI 不能直接写库（必须经 Flow → Repo）
- CLI 不能直接调外部 HTTP（必须经 RunContext 中的 client trait）
- CLI 不能持有业务状态（每次调用都新构造 RunContext）

唯一例外：`migrate` / `validate-config` 这两个纯配置 / 存储工具命令，绕过 Flow 层。

## 7. 当前实现入口

| 内容 | 路径 |
|---|---|
| 二进制入口 | [`src/main.rs`](../../src/main.rs) |
| CLI 路由 | [`crates/cli/src/lib.rs`](../../crates/cli/src/lib.rs) |
| 子命令实现 | [`crates/cli/src/commands/`](../../crates/cli/src/commands/) |
| RunContext | [`crates/runtime/src/context.rs`](../../crates/runtime/src/context.rs) |
| Flow 模块集合 | [`crates/runtime/src/flows/`](../../crates/runtime/src/flows/) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。
