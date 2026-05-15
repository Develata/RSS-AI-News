//! `reindex` command — 规则升级触发的批量重算。
//!
//! 参数语义见 docs/design/cli-semantics.md §4.8：
//! - `--target` ∈ {link_hash, content_hash, categories, all}
//!   `all` 顺序执行三类，每类提交独立 job（§4.8 line 287, 297）
//! - `--abort <job_id>`：取消指定 job，与 `--target` 互斥（line 290）。
//!   F15-10 起接通 storage abort：`pending`/`running` → `aborted`，幂等
//! - `--dry-run`：仅统计，不写表（line 289 / 325）。F15-10 起接通 runtime
//!   dry-run 路径，scanned/would-update/unchanged/conflict_skipped/archived/
//!   errors 与真实 run 数字一致
//!
//! 多 target 顺序执行时，任一 target 失败立即停止后续 target（与 §4.8 line 308
//! 一致：reindex_job 互斥语义在 storage 层兜底；CLI 层只负责调度顺序）。

use std::io::{self, Write};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{
    ReindexAbortOutcome, ReindexFlow, ReindexOptions, ReindexSummary,
    ReindexTarget as DomainReindexTarget,
};
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
    /// `--target=all` 时含 3 项；单 target 模式含 1 项。`--abort` 与
    /// `--dry-run` 都用 1 项概括，靠 `mode` 字段区分。
    pub mode: ReindexMode,
    pub per_target: Vec<ReindexTargetOutcome>,
    /// `mode = Abort` 时填入；其它模式为 `None`。
    pub abort: Option<ReindexAbortReport>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReindexMode {
    Run,
    DryRun,
    Abort,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReindexTargetOutcome {
    pub target: String,
    /// dry-run 模式下保持 0（不写 rule_versions）。
    pub new_rule_version_id: i64,
    /// dry-run 模式下保持 0（不写 reindex_jobs）。F15-7 + F15-10。
    pub reindex_job_id: i64,
    pub scanned: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub conflict_skipped: u32,
    pub archived: u32,
    pub errors: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReindexAbortReport {
    pub job_id: i64,
    pub aborted: bool,
    pub target: Option<String>,
    pub previous_state: Option<String>,
}

impl CommandSummary for ReindexCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        match self.mode {
            ReindexMode::Abort => {
                let report = self
                    .abort
                    .as_ref()
                    .expect("abort mode summary must carry abort report");
                if report.aborted {
                    writeln!(
                        writer,
                        "Aborted reindex job {} (target={}, previous_state={})",
                        report.job_id,
                        report.target.as_deref().unwrap_or("?"),
                        report.previous_state.as_deref().unwrap_or("?"),
                    )?;
                } else {
                    let detail = match (&report.target, &report.previous_state) {
                        (Some(t), Some(s)) => {
                            format!("target={t}, current_state={s}")
                        }
                        _ => "job not found".to_string(),
                    };
                    writeln!(
                        writer,
                        "No reindex job to abort: job_id={} ({})",
                        report.job_id, detail
                    )?;
                }
            }
            ReindexMode::Run | ReindexMode::DryRun => {
                let header = match self.mode {
                    ReindexMode::DryRun => "Reindex dry-run summary:",
                    _ => "Reindex completed:",
                };
                writeln!(writer, "{header}")?;
                for outcome in &self.per_target {
                    writeln!(writer, "  Target:            {}", outcome.target)?;
                    if self.mode == ReindexMode::DryRun {
                        writeln!(writer, "  Job id:            (dry-run, none)")?;
                        writeln!(writer, "  Rule version:      (dry-run, none)")?;
                    } else {
                        writeln!(writer, "  Job id:            {}", outcome.reindex_job_id)?;
                        writeln!(
                            writer,
                            "  Rule version:      {}",
                            outcome.new_rule_version_id
                        )?;
                    }
                    writeln!(writer, "  Scanned:           {}", outcome.scanned)?;
                    let updated_label = if self.mode == ReindexMode::DryRun {
                        "Would update:     "
                    } else {
                        "Updated:          "
                    };
                    writeln!(writer, "  {} {}", updated_label, outcome.updated)?;
                    writeln!(writer, "  Unchanged:         {}", outcome.unchanged)?;
                    writeln!(writer, "  Conflict skipped:  {}", outcome.conflict_skipped)?;
                    writeln!(writer, "  Archived:          {}", outcome.archived)?;
                    writeln!(writer, "  Errors:            {}", outcome.errors)?;
                }
            }
        }
        Ok(())
    }
}

pub async fn run(cli: &Cli, args: &ReindexArgs) -> Result<ReindexCommandSummary, CliError> {
    // §4.8 line 290 abort 分支优先于 target 分支。clap 已通过 conflicts_with
    // 保证两者互斥；这里只需识别 `Some(job_id)` 即可。
    if let Some(raw) = &args.abort {
        let job_id = raw
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| CliError::ReindexAbortInvalidJobId { raw: raw.clone() })?;

        let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
        let (_pool, ctx) = build_run_context("reindex", &loaded).await?;
        let outcome = ReindexFlow::new(ctx)
            .abort(job_id, "cli reindex --abort")
            .await?;

        return Ok(ReindexCommandSummary {
            mode: ReindexMode::Abort,
            per_target: Vec::new(),
            abort: Some(into_abort_report(outcome)),
        });
    }

    // clap `required_unless_present="abort"` 保证 target 必然存在；此处仅为
    // 防御性兜底（例如未来直接构造 ReindexArgs 时绕开 clap 检查）。
    let target = args.target.ok_or(CliError::ReindexTargetRequired)?;
    let domain_targets = target.expand();

    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let (_pool, ctx) = build_run_context("reindex", &loaded).await?;

    // F15-10：dry-run 与真实 run 共用 build_run_context（dry-run 仅读不写，
    // 复用同一 RunContext 没有副作用）。
    let mode = if args.dry_run {
        ReindexMode::DryRun
    } else {
        ReindexMode::Run
    };

    let mut outcomes = Vec::with_capacity(domain_targets.len());
    for target in domain_targets {
        let flow = ReindexFlow::new(ctx.clone());
        let outcome = run_single_target(
            flow,
            target,
            args.batch_size,
            categories.clone(),
            args.dry_run,
        )
        .await?;
        outcomes.push(outcome);
    }

    Ok(ReindexCommandSummary {
        mode,
        per_target: outcomes,
        abort: None,
    })
}

async fn run_single_target(
    flow: ReindexFlow,
    target: DomainReindexTarget,
    batch_size: u32,
    categories: Vec<CategoryConfig>,
    dry_run: bool,
) -> Result<ReindexTargetOutcome, CliError> {
    let target_str = target.to_string();
    let tag = format!(
        "reindex-{}-{}",
        target_str,
        time::OffsetDateTime::now_utc().unix_timestamp()
    );
    let opts = ReindexOptions {
        target,
        batch_size,
        categories,
        new_rule_version_tag: tag,
        new_rule_version_description: format!("manual reindex for {target_str}"),
        new_rule_version_sha256: sha256_hex(target_str.as_bytes()),
    };
    let summary: ReindexSummary = if dry_run {
        flow.dry_run(opts).await?
    } else {
        flow.run(opts).await?
    };

    Ok(ReindexTargetOutcome {
        target: target_str,
        new_rule_version_id: summary.new_rule_version_id,
        reindex_job_id: summary.reindex_job_id,
        scanned: summary.scanned,
        updated: summary.updated,
        unchanged: summary.unchanged,
        conflict_skipped: summary.conflict_skipped,
        archived: summary.archived,
        errors: summary.errors,
    })
}

fn into_abort_report(outcome: ReindexAbortOutcome) -> ReindexAbortReport {
    ReindexAbortReport {
        job_id: outcome.job_id,
        aborted: outcome.aborted,
        target: outcome.target,
        previous_state: outcome.previous_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Cli, Command, LogFormat, OutputFormat};
    use crate::exit_code::ExitCode;
    use std::path::PathBuf;

    fn cli_for(args: ReindexArgs) -> Cli {
        Cli {
            config_dir: PathBuf::from("configs"),
            db_path: None,
            log_level: "info".to_string(),
            log_format: LogFormat::Pretty,
            log_file: String::new(),
            metrics_bind: String::new(),
            output_format: OutputFormat::Pretty,
            dry_run: false,
            category: None,
            timezone: None,
            command: Command::Reindex(args.clone()),
        }
    }

    #[tokio::test]
    async fn abort_non_integer_job_id_short_circuits_with_user_error() {
        // F15-10 W9-F4: --abort 解析非法 job_id → UserError，short-circuit
        // 在 build_run_context 之前。证据：config_dir 给一个不存在路径，仍能
        // 跑通到 ReindexAbortInvalidJobId（若解析在 config 加载之后会先报
        // ConfigError）。
        let args = ReindexArgs {
            target: None,
            batch_size: 100,
            abort: Some("not-a-number".to_string()),
            dry_run: false,
        };
        let mut cli = cli_for(args.clone());
        cli.config_dir = PathBuf::from("/nonexistent/path/that/should/never/be/loaded");

        let err = run(&cli, &args).await.expect_err("abort returns error");
        match &err {
            CliError::ReindexAbortInvalidJobId { raw } => assert_eq!(raw, "not-a-number"),
            other => panic!("expected ReindexAbortInvalidJobId, got {other:?}"),
        }
        assert_eq!(err.error_kind(), "reindex_abort_invalid_job_id");
        assert!(matches!(err.exit_code(), ExitCode::UserError));
    }

    #[tokio::test]
    async fn abort_zero_or_negative_job_id_is_rejected() {
        // job id 必须 > 0；0 与负数同样视为非法。
        for raw in ["0", "-5"] {
            let args = ReindexArgs {
                target: None,
                batch_size: 100,
                abort: Some(raw.to_string()),
                dry_run: false,
            };
            let mut cli = cli_for(args.clone());
            cli.config_dir = PathBuf::from("/nonexistent/path");
            let err = run(&cli, &args)
                .await
                .expect_err("invalid job_id returns error");
            assert!(matches!(err, CliError::ReindexAbortInvalidJobId { .. }));
        }
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
            mode: ReindexMode::Run,
            abort: None,
            per_target: vec![
                ReindexTargetOutcome {
                    target: "link_hash".to_string(),
                    new_rule_version_id: 1,
                    reindex_job_id: 10,
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
                    reindex_job_id: 11,
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
                    reindex_job_id: 12,
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
        assert!(
            text.contains("Job id:            10"),
            "F15-10: pretty 必须含 job_id 行；得到:\n{text}"
        );
        assert!(text.contains("Job id:            11"));
        assert!(text.contains("Job id:            12"));
    }

    #[test]
    fn dry_run_pretty_replaces_job_id_with_placeholder_and_would_update_label() {
        // F15-10: dry-run summary 用 "Would update" 与占位 "(dry-run, none)"。
        let summary = ReindexCommandSummary {
            mode: ReindexMode::DryRun,
            abort: None,
            per_target: vec![ReindexTargetOutcome {
                target: "link_hash".to_string(),
                new_rule_version_id: 0,
                reindex_job_id: 0,
                scanned: 10,
                updated: 5,
                unchanged: 5,
                conflict_skipped: 0,
                archived: 0,
                errors: 0,
            }],
        };
        let mut buf = Vec::new();
        summary.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.starts_with("Reindex dry-run summary"));
        assert!(text.contains("Job id:            (dry-run, none)"));
        assert!(text.contains("Rule version:      (dry-run, none)"));
        assert!(text.contains("Would update:"));
    }

    #[test]
    fn abort_pretty_summary_distinguishes_aborted_and_no_op() {
        // F15-10: abort 模式 pretty 输出两条主线 —— 真 abort 与 noop。
        let success = ReindexCommandSummary {
            mode: ReindexMode::Abort,
            per_target: Vec::new(),
            abort: Some(ReindexAbortReport {
                job_id: 42,
                aborted: true,
                target: Some("link_hash".to_string()),
                previous_state: Some("running".to_string()),
            }),
        };
        let mut buf = Vec::new();
        success.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Aborted reindex job 42"));
        assert!(text.contains("target=link_hash"));
        assert!(text.contains("previous_state=running"));

        let noop = ReindexCommandSummary {
            mode: ReindexMode::Abort,
            per_target: Vec::new(),
            abort: Some(ReindexAbortReport {
                job_id: 99,
                aborted: false,
                target: None,
                previous_state: None,
            }),
        };
        let mut buf = Vec::new();
        noop.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("No reindex job to abort"));
        assert!(text.contains("job_id=99"));
    }
}
