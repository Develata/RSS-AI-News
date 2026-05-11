//! `reindex` command — 规则升级触发的批量重算。
//!
//! 参数语义见 docs/design/cli-semantics.md §4.8：
//! - `--target` ∈ {link_hash, content_hash, categories, all}
//!   `all` 顺序执行三类，每类提交独立 job（§4.8 line 287, 297）
//! - `--abort <job_id>`：取消指定 job，与 `--target` 互斥（line 290）
//!   注：当前 storage 层未实现 `reindex_jobs` 表，runtime 无 abort 接口；
//!   CLI 层接受 flag 并以 `ReindexAbortNotImplemented` 退出，满足 §4.8 表面契约
//! - `--dry-run`：仅统计，不写表（line 289）；同样为 NotImplemented，待 W3
//!
//! 多 target 顺序执行时，任一 target 失败立即停止后续 target（与 §4.8 line 308
//! 一致：reindex_job 互斥语义在 storage 层兜底；CLI 层只负责调度顺序）。

use std::io::{self, Write};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{ReindexFlow, ReindexOptions, ReindexTarget as DomainReindexTarget};
use serde::Serialize;

use crate::{
    args::{Cli, ReindexArgs},
    commands::backfill::sha256_hex,
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct ReindexCommandSummary {
    /// `--target=all` 时含 3 项；单 target 模式含 1 项。
    pub per_target: Vec<ReindexTargetOutcome>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReindexTargetOutcome {
    pub target: String,
    pub new_rule_version_id: i64,
    pub scanned: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub conflict_skipped: u32,
    pub archived: u32,
    pub errors: u32,
}

impl CommandSummary for ReindexCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Reindex completed:")?;
        for outcome in &self.per_target {
            writeln!(writer, "  Target:            {}", outcome.target)?;
            writeln!(writer, "  Rule version:      {}", outcome.new_rule_version_id)?;
            writeln!(writer, "  Scanned:           {}", outcome.scanned)?;
            writeln!(writer, "  Updated:           {}", outcome.updated)?;
            writeln!(writer, "  Unchanged:         {}", outcome.unchanged)?;
            writeln!(writer, "  Conflict skipped:  {}", outcome.conflict_skipped)?;
            writeln!(writer, "  Archived:          {}", outcome.archived)?;
            writeln!(writer, "  Errors:            {}", outcome.errors)?;
        }
        Ok(())
    }
}

pub async fn run(cli: &Cli, args: &ReindexArgs) -> Result<ReindexCommandSummary, CliError> {
    // §4.8 line 290 abort 分支优先于 target 分支。clap 已通过 conflicts_with
    // 保证两者互斥；这里只需识别 `Some(job_id)` 即可。
    if let Some(job_id) = &args.abort {
        return Err(CliError::ReindexAbortNotImplemented {
            job_id: job_id.clone(),
        });
    }

    // §4.8 line 289: `--dry-run` 仅统计、不写。当前 ReindexFlow 缺 dry-run
    // 入口；surface 接受 flag 并以 NotImplemented 退出，待 W3 落地。
    if args.dry_run {
        return Err(CliError::ReindexDryRunNotImplemented);
    }

    // clap `required_unless_present="abort"` 保证 target 必然存在；此处仅为
    // 防御性兜底（例如未来直接构造 ReindexArgs 时绕开 clap 检查）。
    let target = args.target.ok_or(CliError::ReindexTargetRequired)?;
    let domain_targets = target.expand();

    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let (_pool, ctx) = build_run_context("reindex", &loaded).await?;

    let mut outcomes = Vec::with_capacity(domain_targets.len());
    for target in domain_targets {
        let outcome = run_single_target(
            ReindexFlow::new(ctx.clone()),
            target,
            args.batch_size,
            categories.clone(),
        )
        .await?;
        outcomes.push(outcome);
    }

    Ok(ReindexCommandSummary {
        per_target: outcomes,
    })
}

async fn run_single_target(
    flow: ReindexFlow,
    target: DomainReindexTarget,
    batch_size: u32,
    categories: Vec<CategoryConfig>,
) -> Result<ReindexTargetOutcome, CliError> {
    let target_str = target.to_string();
    let tag = format!(
        "reindex-{}-{}",
        target_str,
        time::OffsetDateTime::now_utc().unix_timestamp()
    );
    let summary = flow
        .run(ReindexOptions {
            target,
            batch_size,
            categories,
            new_rule_version_tag: tag,
            new_rule_version_description: format!("manual reindex for {target_str}"),
            new_rule_version_sha256: sha256_hex(target_str.as_bytes()),
        })
        .await?;

    Ok(ReindexTargetOutcome {
        target: target_str,
        new_rule_version_id: summary.new_rule_version_id,
        scanned: summary.scanned,
        updated: summary.updated,
        unchanged: summary.unchanged,
        conflict_skipped: summary.conflict_skipped,
        archived: summary.archived,
        errors: summary.errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{
        Cli, Command, LogFormat, OutputFormat, ReindexTarget as CliReindexTarget,
    };
    use crate::exit_code::ExitCode;
    use std::path::PathBuf;

    fn cli_for(args: ReindexArgs) -> Cli {
        Cli {
            config_dir: PathBuf::from("configs"),
            db_path: None,
            log_level: "info".to_string(),
            log_format: LogFormat::Pretty,
            output_format: OutputFormat::Pretty,
            dry_run: false,
            category: None,
            timezone: None,
            max_batches: None,
            command: Command::Reindex(args.clone()),
        }
    }

    #[tokio::test]
    async fn abort_branch_short_circuits_before_config_load() {
        // §4.8 line 290: --abort 仅取消 job，不应触发 reindex 流程。
        // 当前 storage 尚未实现 reindex_jobs，CLI surface 返回 NotImplemented
        // 而非真正去做配置加载 / DB 连接 —— 用一个不存在的 config_dir 也能
        // 跑通这条路径，反证 abort 是 short-circuit。
        let args = ReindexArgs {
            target: None,
            batch_size: 100,
            abort: Some("job-123".to_string()),
            dry_run: false,
        };
        let mut cli = cli_for(args.clone());
        cli.config_dir = PathBuf::from("/nonexistent/path/that/should/never/be/loaded");

        let err = run(&cli, &args).await.expect_err("abort returns error");
        match &err {
            CliError::ReindexAbortNotImplemented { job_id } => {
                assert_eq!(job_id, "job-123");
            }
            other => panic!("expected ReindexAbortNotImplemented, got {other:?}"),
        }
        assert_eq!(err.error_kind(), "reindex_abort_not_implemented");
        assert!(matches!(err.exit_code(), ExitCode::RuntimeError));
    }

    #[tokio::test]
    async fn dry_run_branch_short_circuits_before_config_load() {
        // §4.8 line 289: --dry-run 不写任何表。当前未实现，CLI surface
        // 返回 NotImplemented；同样 short-circuit。
        let args = ReindexArgs {
            target: Some(CliReindexTarget::LinkHash),
            batch_size: 100,
            abort: None,
            dry_run: true,
        };
        let mut cli = cli_for(args.clone());
        cli.config_dir = PathBuf::from("/nonexistent/path");

        let err = run(&cli, &args).await.expect_err("dry-run returns error");
        assert!(matches!(err, CliError::ReindexDryRunNotImplemented));
        assert_eq!(err.error_kind(), "reindex_dry_run_not_implemented");
    }

    #[test]
    fn reindex_target_required_maps_to_exit_code_user_error() {
        // §4.8 line 327: 参数错误 → exit 2。
        let err = CliError::ReindexTargetRequired;
        assert!(matches!(err.exit_code(), ExitCode::UserError));
    }

    #[test]
    fn summary_pretty_lists_each_target_section_for_all() {
        // §4.8 line 297: target='all' 顺序展开三类；输出应分别列出三个 section。
        let summary = ReindexCommandSummary {
            per_target: vec![
                ReindexTargetOutcome {
                    target: "link_hash".to_string(),
                    new_rule_version_id: 1,
                    scanned: 10,
                    updated: 5,
                    unchanged: 5,
                    conflict_skipped: 0,
                    archived: 0,
                    errors: 0,
                },
                ReindexTargetOutcome {
                    target: "content_hash".to_string(),
                    new_rule_version_id: 2,
                    scanned: 0,
                    updated: 0,
                    unchanged: 0,
                    conflict_skipped: 0,
                    archived: 0,
                    errors: 0,
                },
                ReindexTargetOutcome {
                    target: "categories".to_string(),
                    new_rule_version_id: 3,
                    scanned: 7,
                    updated: 7,
                    unchanged: 0,
                    conflict_skipped: 0,
                    archived: 1,
                    errors: 0,
                },
            ],
        };
        let mut buf = Vec::new();
        summary.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Target:            link_hash"));
        assert!(text.contains("Target:            content_hash"));
        assert!(text.contains("Target:            categories"));
    }
}
