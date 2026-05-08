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

#[derive(Args, Debug, Clone)]
pub struct ReindexArgs {
    #[arg(long, value_enum)]
    pub target: ReindexTarget,
    #[arg(long = "batch-size", default_value_t = 100)]
    pub batch_size: u32,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexTarget {
    LinkHash,
    ContentHash,
    Categories,
}

impl From<ReindexTarget> for DomainReindexTarget {
    fn from(value: ReindexTarget) -> Self {
        match value {
            ReindexTarget::LinkHash => Self::LinkHash,
            ReindexTarget::ContentHash => Self::ContentHash,
            ReindexTarget::Categories => Self::Categories,
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
