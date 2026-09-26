use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rss_ai_news_config::CliOverrides;
use rss_ai_news_domain::state::ReindexTarget as DomainReindexTarget;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Help text (`///`) is user-facing; implementation notes stay in `//`.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "rss-ai-news",
    version,
    about = "抓取 RSS/Atom/JSON Feed，AI 摘要筛选，生成 Markdown 日报并发布到本地或 GitHub",
    long_about = "抓取 RSS/Atom/JSON Feed → 抓正文 → AI 摘要与筛选 → Markdown 日报 → 本地或 GitHub 发布。\n\
                  一次性运行后退出，由 cron / systemd timer / 容器调度器定时触发。\n\n\
                  首次使用：validate-config → migrate run → run；排障：doctor。",
    after_help = "退出码：0 成功（含“当天暂无内容”）· 1 运行失败 · 2 参数错误 · 78 配置错误"
)]
pub struct Cli {
    /// 配置目录（含 app.toml 与 categories/*.toml）
    #[arg(short = 'c', long = "config-dir", default_value = "configs")]
    pub config_dir: PathBuf,

    /// SQLite 数据库文件路径，覆盖 [database].sqlite_path
    #[arg(long = "db-path", value_name = "PATH")]
    pub db_path: Option<PathBuf>,

    /// 日志级别：trace / debug / info / warn / error
    #[arg(long = "log-level", default_value = "info", value_name = "LEVEL")]
    pub log_level: String,

    /// 日志格式（写 stderr）
    #[arg(long = "log-format", default_value = "pretty", value_enum)]
    pub log_format: LogFormat,

    // Tracing is initialised before config.toml is read, so
    // `[observability].log_file` only takes effect through this flag.
    /// 同时把日志写入按天轮转的文件（<PATH>.YYYY-MM-DD）；留空只写 stderr
    #[arg(
        long = "log-file",
        default_value = "",
        value_name = "PATH",
        hide_default_value = true
    )]
    pub log_file: String,

    // Same startup-order limitation as --log-file for
    // `[observability].metrics_bind`.
    /// 在该地址提供 Prometheus /metrics（如 127.0.0.1:9090）；留空不启动
    #[arg(
        long = "metrics-bind",
        default_value = "",
        value_name = "ADDR",
        hide_default_value = true
    )]
    pub metrics_bind: String,

    /// 结果输出格式；json 时 stdout 恰好一个 JSON 文档，status 与退出码一致
    #[arg(
        short = 'o',
        long = "output-format",
        default_value = "pretty",
        value_enum
    )]
    pub output_format: OutputFormat,

    /// 只预演：reindex 支持；只读命令视为无操作；其余命令以参数错误拒绝（显式的 --log-file / --metrics-bind 照常生效）
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// 只处理该分类（categories/*.toml 中的 [category].key）
    #[arg(short = 'C', long = "category", value_name = "KEY")]
    pub category: Option<String>,

    /// 报告日期所用时区（IANA 名称），覆盖 [publish].target_timezone
    #[arg(long = "timezone", value_name = "TZ")]
    pub timezone: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// 把 CLI 参数折叠为 `CliOverrides`。
    ///
    /// `--max-batches` 仅在 [`IngestArgs`] / [`AiRunArgs`] / [`RunArgs`] 三个
    /// 子命令暴露，其余子命令的 overrides 该字段固定为 `None`。
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
    /// 便于人读
    Pretty,
    /// 结构化，便于 jq / 日志系统
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
    /// 便于人读的摘要
    Pretty,
    /// 单个 JSON 文档，便于脚本 / 调度器判断
    Json,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// 抓取 feed 并提取正文（不需要 AI 凭据）
    Ingest(IngestArgs),
    /// 对待分析文章做 AI 摘要、打分与筛选
    AiRun(AiRunArgs),
    /// 生成并发布单个分类的日报（需 --category 或仅有一个分类）
    Publish(PublishArgs),
    /// 生成并发布所有分类的日报；远端发布合并为一次 commit
    PublishAll(PublishArgs),
    /// 健康检查：配置、数据库、外部依赖；--deep 额外校验数据不变量
    Doctor(DoctorArgs),
    /// 离线重放已留档的 feed / HTML / AI 原始输入
    Replay(ReplayArgs),
    /// 按日期范围重新提取正文或重跑 AI（新版本并存，不覆盖旧结果）
    Backfill(BackfillArgs),
    /// 从已冻结的快照重新渲染历史日报（不抓取、不调 AI）
    RebuildReport(RebuildReportArgs),
    /// 规则升级后重算去重 hash / 分类归属
    Reindex(ReindexArgs),
    /// 只读导出近期候选条目与订阅源健康度（供下游使用）
    RecentEntries(RecentEntriesArgs),
    /// 初始化或检查数据库结构
    Migrate(MigrateArgs),
    /// 只校验配置与 .env，不连库、不访问网络
    ValidateConfig,
    /// 完整流程：ingest → ai-run（AI 关闭时跳过）→ publish-all
    Run(RunArgs),
}

impl Command {
    /// Subcommand name as typed on the command line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ingest(_) => "ingest",
            Self::AiRun(_) => "ai-run",
            Self::Publish(_) => "publish",
            Self::PublishAll(_) => "publish-all",
            Self::Doctor(_) => "doctor",
            Self::Replay(_) => "replay",
            Self::Backfill(_) => "backfill",
            Self::RebuildReport(_) => "rebuild-report",
            Self::Reindex(_) => "reindex",
            Self::RecentEntries(_) => "recent-entries",
            Self::Migrate(_) => "migrate",
            Self::ValidateConfig => "validate-config",
            Self::Run(_) => "run",
        }
    }

    /// Whether the global `--dry-run` may be combined with this command:
    /// `reindex` implements it (read-only pool); commands without any side
    /// effect treat it as a no-op. Everything else is rejected, because
    /// silently ignoring the flag would perform the side effects the user
    /// asked to skip: writes, `migrate check`'s writable (file-creating)
    /// connection, or `doctor`'s real API probes.
    pub fn accepts_dry_run(&self) -> bool {
        match self {
            // Read-only pools, no network.
            Self::Reindex(_) | Self::ValidateConfig | Self::RecentEntries(_) | Self::Replay(_) => {
                true
            }
            Self::RebuildReport(args) => args.output.is_none(),
            Self::Doctor(_)
            | Self::Migrate(_)
            | Self::Ingest(_)
            | Self::AiRun(_)
            | Self::Publish(_)
            | Self::PublishAll(_)
            | Self::Backfill(_)
            | Self::Run(_) => false,
        }
    }
}

#[derive(Args, Debug, Clone, Default)]
pub struct IngestArgs {
    /// 只抓取 feed 条目，不抓正文
    #[arg(long = "skip-fetch")]
    pub skip_fetch: bool,
    /// 每批领取的正文抓取任务数
    #[arg(long = "batch-size", default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub batch_size: u32,
    /// 本次最多处理的批数，覆盖 [runtime].max_batches_per_run；0 = 不限
    #[arg(long = "max-batches", value_name = "N")]
    pub max_batches: Option<u32>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct AiRunArgs {
    /// 每批领取的 AI 任务数
    #[arg(long = "batch-size", default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub batch_size: u32,
    /// 本次使用的模型，覆盖分类 / 全局配置
    #[arg(long)]
    pub model: Option<String>,
    /// 每个分类最多处理的批数（多分类时按分类分别计），覆盖 [runtime].max_batches_per_run；0 = 不限
    #[arg(long = "max-batches", value_name = "N")]
    pub max_batches: Option<u32>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct PublishArgs {
    /// 报告日期 YYYY-MM-DD，默认今天（UTC）
    #[arg(long)]
    pub date: Option<String>,
    /// 只写本地目录，不推 GitHub
    #[arg(long = "local-only")]
    pub local_only: bool,
    /// 为同一天再生成一份新的发布批次（旧记录保留）
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct DoctorArgs {
    /// 额外扫描数据库不变量（大库可能需要数秒以上）
    #[arg(long)]
    pub deep: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ReplayArgs {
    /// 要重放的留档类型
    #[arg(long, value_enum)]
    pub kind: ReplayKind,
    /// 按留档 key 选择
    #[arg(long, conflicts_with = "id")]
    pub key: Option<String>,
    /// 按留档 id 选择
    #[arg(long, conflicts_with = "key")]
    pub id: Option<i64>,
    /// 与入库结果对比（目前仅 html：内容 hash 与字数）
    #[arg(long)]
    pub diff: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayKind {
    /// feed 原文
    Feed,
    /// 详情页 HTML
    Html,
    /// AI 原始响应
    Ai,
}

// `--target ai` inserts new `article_ai_results` rows carrying new version
// metadata and never overwrites old ones; the three override flags below only
// apply to `--target ai` and are ignored for `--target extract`.
#[derive(Args, Debug, Clone)]
pub struct BackfillArgs {
    /// 重跑正文提取还是 AI 分析
    #[arg(long, value_enum)]
    pub target: BackfillTarget,
    /// 起始日期 YYYY-MM-DD（含）
    #[arg(long = "date-from")]
    pub date_from: Option<String>,
    /// 结束日期 YYYY-MM-DD（含）
    #[arg(long = "date-to")]
    pub date_to: Option<String>,
    /// 每批处理条数
    #[arg(long = "batch-size", default_value_t = 50)]
    pub batch_size: u32,
    /// 新 prompt 版本的标签（仅 --target ai；默认 backfill-<时间戳>）
    #[arg(long = "prompt-version-tag")]
    pub prompt_version_tag: Option<String>,
    /// 新 prompt 版本的说明（仅 --target ai）
    #[arg(long = "prompt-version-description")]
    pub prompt_version_description: Option<String>,
    /// 本次使用的模型（仅 --target ai；默认 [ai].model）
    #[arg(long = "model")]
    pub model: Option<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillTarget {
    /// 重新提取正文
    Extract,
    /// 重跑 AI 分析
    Ai,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RebuildReportArgs {
    /// 发布记录 id
    #[arg(long = "publish-id", conflicts_with_all = ["date"])]
    pub publish_id: Option<i64>,
    /// 报告日期 YYYY-MM-DD（配合 --category）
    #[arg(long, conflicts_with = "publish_id")]
    pub date: Option<String>,
    /// 写入该文件；省略时 Markdown 输出到 stdout
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct RecentEntriesArgs {
    /// 只导出此时间之后发现的条目（RFC3339，如 2026-07-01T00:00:00Z）
    #[arg(long = "discovered-after", value_parser = parse_rfc3339)]
    pub discovered_after: OffsetDateTime,
    /// 可选：再按文章发布时间过滤（RFC3339）；默认不过滤
    #[arg(long = "published-after", value_parser = parse_rfc3339)]
    pub published_after: Option<OffsetDateTime>,
    /// 最多导出条数
    #[arg(
        long,
        default_value_t = rss_ai_news_runtime::DEFAULT_RECENT_ENTRIES_LIMIT,
        value_parser = clap::value_parser!(u32).range(1..=200)
    )]
    pub limit: u32,
}

fn parse_rfc3339(value: &str) -> Result<OffsetDateTime, String> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|error| format!("invalid RFC3339 timestamp {value:?}: {error}"))
}

// `--target` and `--abort` are mutually exclusive and exactly one is
// required (`conflicts_with` + `required_unless_present`).
#[derive(Args, Debug, Clone)]
pub struct ReindexArgs {
    /// 重算目标（与 --abort 二选一）
    #[arg(
        long,
        value_enum,
        required_unless_present = "abort",
        conflicts_with = "abort"
    )]
    pub target: Option<ReindexTarget>,
    /// 每批处理条数
    #[arg(long = "batch-size", default_value_t = 100)]
    pub batch_size: u32,
    /// 取消指定 id 的 reindex 任务（与 --target 二选一）
    #[arg(long = "abort", conflicts_with = "target", value_name = "JOB_ID")]
    pub abort: Option<String>,
    /// 只统计将要更新的行数，不写任何表
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

// `All` runs link_hash → content_hash → categories as three separate jobs;
// the other values map 1:1 to [`DomainReindexTarget`].
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "snake_case")]
pub enum ReindexTarget {
    /// 按当前链接规范化规则重算链接 hash
    LinkHash,
    /// 重算正文内容 hash
    ContentHash,
    /// 按当前配置重新归属分类（下线源归档）
    Categories,
    /// 依次执行以上三项
    All,
}

impl ReindexTarget {
    /// 把 CLI 选项展开为底层 domain target 序列。`All` 展开为
    /// `[LinkHash, ContentHash, Categories]`。
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
    /// 应用尚未执行的迁移（幂等）
    Run,
    /// 只检查迁移是否已全部应用，不写库
    Check,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    /// ingest 阶段每批任务数（默认 50）
    #[arg(long = "ingest-batch-size", value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub ingest_batch_size: Option<u32>,
    /// ai-run 阶段每批任务数（默认 20）
    #[arg(long = "ai-batch-size", value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub ai_batch_size: Option<u32>,
    /// 发布的报告日期 YYYY-MM-DD，默认今天（UTC）
    #[arg(long = "publish-date")]
    pub publish_date: Option<String>,
    /// 最多处理的批数：ingest 计一份，ai-run 每个分类各计一份；0 = 不限（publish 不受影响）
    #[arg(long = "max-batches", value_name = "N")]
    pub max_batches: Option<u32>,
}
