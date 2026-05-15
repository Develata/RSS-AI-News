use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rss_ai_news_config::CliOverrides;
use rss_ai_news_domain::state::ReindexTarget as DomainReindexTarget;

#[derive(Parser, Debug, Clone)]
#[command(name = "rss-ai-news", version, about = "Rust 版 RSS-AI-News CLI")]
pub struct Cli {
    #[arg(short = 'c', long = "config-dir", default_value = "configs")]
    pub config_dir: PathBuf,

    #[arg(long = "db-path")]
    pub db_path: Option<PathBuf>,

    #[arg(long = "log-level", default_value = "info")]
    pub log_level: String,

    #[arg(long = "log-format", default_value = "pretty", value_enum)]
    pub log_format: LogFormat,

    /// 日志落盘路径（F15-13 W9-F1）。空串 → 仅 stderr；非空 → 用
    /// `tracing_appender::rolling::daily` 按 `<prefix>.YYYY-MM-DD` 日轮转。
    /// 解析规则见 `rss_ai_news_observability::tracing_init::InitOptions::log_file`。
    /// startup init 在 config.toml 读取之前发生，所以 `[observability].log_file`
    /// 当前仅能通过本标志生效（与 `--log-level` / `--log-format` 行为对齐）。
    #[arg(long = "log-file", default_value = "")]
    pub log_file: String,

    /// Prometheus `/metrics` HTTP 端点绑定地址（F15-14 W9-F2）。
    /// 空串 → 不启动 metrics server；非空（如 `127.0.0.1:9090`）→
    /// 启动后台 tokio task，挂在该 `SocketAddr` 上提供 `/metrics`。
    /// 与 `--log-file` 同源限制：CLI startup 早于 config.toml 加载，
    /// `[observability].metrics_bind` 当前仅能通过本标志生效。
    #[arg(long = "metrics-bind", default_value = "")]
    pub metrics_bind: String,

    #[arg(
        short = 'o',
        long = "output-format",
        default_value = "pretty",
        value_enum
    )]
    pub output_format: OutputFormat,

    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    #[arg(short = 'C', long = "category")]
    pub category: Option<String>,

    #[arg(long = "timezone")]
    pub timezone: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// 把 CLI 参数折叠为 `CliOverrides`。
    ///
    /// `--max-batches` 仅在 [`IngestArgs`] / [`AiRunArgs`] / [`RunArgs`] 三个
    /// 子命令暴露（cli-semantics.md §4.1 / §4.2 / §4.11；config-schema.md
    /// §8 line 405），其余子命令的 overrides 该字段固定为 `None`。F7-1
    /// 修复：此前以 `global = true` 形式挂在 [`Cli`] 上，导致 `publish`、
    /// `doctor` 等子命令 `--help` 也显示该标志（W3-2 surface drift）。
    pub fn to_cli_overrides(&self) -> CliOverrides {
        let max_batches = match &self.command {
            Command::Ingest(args) => args.max_batches,
            Command::AiRun(args) => args.max_batches,
            Command::Run(args) => args.max_batches,
            _ => None,
        };
        CliOverrides {
            db_path: self.db_path.clone(),
            log_level: Some(self.log_level.clone()),
            log_format: Some(self.log_format.as_str().to_string()),
            timezone: self.timezone.clone(),
            category_filter: self.category.clone(),
            dry_run: self.dry_run,
            max_batches,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

impl LogFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pretty => "pretty",
            Self::Json => "json",
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Pretty,
    Json,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    Ingest(IngestArgs),
    AiRun(AiRunArgs),
    Publish(PublishArgs),
    Doctor(DoctorArgs),
    Replay(ReplayArgs),
    Backfill(BackfillArgs),
    RebuildReport(RebuildReportArgs),
    Reindex(ReindexArgs),
    Migrate(MigrateArgs),
    ValidateConfig,
    Run(RunArgs),
}

#[derive(Args, Debug, Clone, Default)]
pub struct IngestArgs {
    #[arg(long)]
    pub source: Option<String>,
    #[arg(long = "skip-fetch")]
    pub skip_fetch: bool,
    #[arg(long = "batch-size", default_value_t = 50)]
    pub batch_size: u32,
    /// 覆盖 `runtime.max_batches_per_run`。`0` = 不限（仅由 lease + 宿主
    /// 超时兜底）。F7-1 修复：从 [`Cli`] 全局 flag 改为子命令本地
    /// （cli-semantics.md §4.1 line 62 + config-schema.md §8 line 405 早已
    /// 规定"仅 ingest/ai-run/run"，clap `global = true` 与该约束相悖）。
    #[arg(long = "max-batches")]
    pub max_batches: Option<u32>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct AiRunArgs {
    #[arg(long = "batch-size", default_value_t = 20)]
    pub batch_size: u32,
    #[arg(long)]
    pub model: Option<String>,
    /// 覆盖 `runtime.max_batches_per_run`。语义与 [`IngestArgs::max_batches`]
    /// 一致；cli-semantics.md §4.2 line 97。
    #[arg(long = "max-batches")]
    pub max_batches: Option<u32>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct PublishArgs {
    #[arg(long)]
    pub date: Option<String>,
    #[arg(long = "local-only")]
    pub local_only: bool,
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct DoctorArgs {
    #[arg(long)]
    pub deep: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ReplayArgs {
    #[arg(long, value_enum)]
    pub kind: ReplayKind,
    #[arg(long, conflicts_with = "id")]
    pub key: Option<String>,
    #[arg(long, conflicts_with = "key")]
    pub id: Option<i64>,
    #[arg(long)]
    pub diff: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayKind {
    Feed,
    Html,
    Ai,
}

/// 参数语义见 docs/design/cli-semantics.md §4.6 + state-machine.md §4.4。
///
/// `--target ai` 分支会创建新一行 `article_ai_results`（带新版本元数据），
/// 不覆盖旧行。下列 3 个 override 字段让"多 model / 多版本并存"
/// （state-machine §4.4 line 262）可被 CLI 明确控制：
///   - `--prompt-version-tag` 让用户命名版本（用于实验对照、idempotent
///     重跑）；缺省时回落到 `backfill-<unix-ts>`，非确定性
///   - `--prompt-version-description` 让审计/事后追溯能看到这次重跑的动机
///   - `--model` 允许在 backfill 时切换模型（A/B 对照、新模型重跑历史）
///
/// 三者均不适用于 `--target extract`，相应分支忽略（不报错，保持
/// CLI 表面对齐 §4.6 文档表格的"参数与 target 解耦"风格）。
#[derive(Args, Debug, Clone)]
pub struct BackfillArgs {
    #[arg(long, value_enum)]
    pub target: BackfillTarget,
    #[arg(long = "date-from")]
    pub date_from: Option<String>,
    #[arg(long = "date-to")]
    pub date_to: Option<String>,
    #[arg(long = "batch-size", default_value_t = 50)]
    pub batch_size: u32,
    /// 显式指定 backfill 创建的新 prompt_version tag。缺省时回落为
    /// `backfill-<unix-ts>`（非确定性）。仅 `--target ai` 生效。
    #[arg(long = "prompt-version-tag")]
    pub prompt_version_tag: Option<String>,
    /// 该 prompt_version 行的描述。缺省 `"manual backfill via CLI"`。
    /// 仅 `--target ai` 生效。
    #[arg(long = "prompt-version-description")]
    pub prompt_version_description: Option<String>,
    /// 覆盖 backfill 使用的 model id。缺省读 `app.toml [ai] model`。
    /// 仅 `--target ai` 生效。
    #[arg(long = "model")]
    pub model: Option<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillTarget {
    Extract,
    Ai,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RebuildReportArgs {
    #[arg(long = "publish-id", conflicts_with_all = ["date"])]
    pub publish_id: Option<i64>,
    #[arg(long, conflicts_with = "publish_id")]
    pub date: Option<String>,
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// cli-semantics §4.8 lines 285-290:
///   `--target` 必填（除非 `--abort`），值 ∈ {link_hash, content_hash, categories, all}
///   `--abort <job_id>`：取消指定 job；与 `--target` 互斥
///
/// clap 表达：
///   - `target` 与 `abort` 通过 `conflicts_with` 互斥
///   - 用户必须二选一：clap `required_unless_present` 在二者间形成 XOR
#[derive(Args, Debug, Clone)]
pub struct ReindexArgs {
    /// 重算目标（`--abort` 模式下省略）。
    #[arg(
        long,
        value_enum,
        required_unless_present = "abort",
        conflicts_with = "abort"
    )]
    pub target: Option<ReindexTarget>,
    #[arg(long = "batch-size", default_value_t = 100)]
    pub batch_size: u32,
    /// 取消指定 `reindex_jobs.id`，状态推进到 `aborted`。详见
    /// cli-semantics §4.8 line 290。
    #[arg(long = "abort", conflicts_with = "target")]
    pub abort: Option<String>,
    /// 仅统计将更新行数与待写入 rule_versions 元数据；不写任何表。
    /// 详见 cli-semantics §4.8 line 289。
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

/// CLI 层 reindex 目标。`All` 触发顺序执行 link_hash / content_hash /
/// categories 三个独立 job（cli-semantics §4.8 line 297）。
/// 其余三个值对应 [`DomainReindexTarget`] 一一映射。
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "snake_case")]
pub enum ReindexTarget {
    LinkHash,
    ContentHash,
    Categories,
    All,
}

impl ReindexTarget {
    /// 把 CLI 选项展开为底层 domain target 序列。`All` 展开为
    /// `[LinkHash, ContentHash, Categories]`（顺序由 §4.8 line 297 规定）。
    pub fn expand(self) -> Vec<DomainReindexTarget> {
        match self {
            Self::LinkHash => vec![DomainReindexTarget::LinkHash],
            Self::ContentHash => vec![DomainReindexTarget::ContentHash],
            Self::Categories => vec![DomainReindexTarget::Categories],
            Self::All => vec![
                DomainReindexTarget::LinkHash,
                DomainReindexTarget::ContentHash,
                DomainReindexTarget::Categories,
            ],
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct MigrateArgs {
    #[command(subcommand)]
    pub action: MigrateAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MigrateAction {
    Run,
    Check,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    #[arg(long = "ingest-batch-size")]
    pub ingest_batch_size: Option<u32>,
    #[arg(long = "ai-batch-size")]
    pub ai_batch_size: Option<u32>,
    #[arg(long = "publish-date")]
    pub publish_date: Option<String>,
    /// 覆盖 `runtime.max_batches_per_run`，内部 ingest / ai-run 阶段
    /// 沿用同一生效值；cli-semantics.md §4.11 line 358。
    /// publish 阶段不消费该值。
    #[arg(long = "max-batches")]
    pub max_batches: Option<u32>,
}
