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

    /// 全局 `--max-batches`：覆盖 `runtime.max_batches_per_run`。
    /// 仅对 `ingest` / `ai-run` / `run` 有意义；其他命令忽略。
    /// `0` = 不限（仅由 lease + 宿主超时兜底）。
    /// 详见 docs/design/config-schema.md §8 line 405、§4.4 line 196。
    #[arg(long = "max-batches", global = true)]
    pub max_batches: Option<u32>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn to_cli_overrides(&self) -> CliOverrides {
        CliOverrides {
            db_path: self.db_path.clone(),
            log_level: Some(self.log_level.clone()),
            log_format: Some(self.log_format.as_str().to_string()),
            timezone: self.timezone.clone(),
            category_filter: self.category.clone(),
            dry_run: self.dry_run,
            max_batches: self.max_batches,
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
}

#[derive(Args, Debug, Clone, Default)]
pub struct AiRunArgs {
    #[arg(long = "batch-size", default_value_t = 20)]
    pub batch_size: u32,
    #[arg(long)]
    pub model: Option<String>,
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
    #[arg(long, value_enum, required_unless_present = "abort", conflicts_with = "abort")]
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
}
