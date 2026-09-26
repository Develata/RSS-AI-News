//! `run` command — orchestrates ingest → ai-run → publish in one invocation.
//!
//! Per `docs/design/cli-semantics.md` §4.11: any single stage failing must
//! NOT block downstream stages (with the explicit exception that an
//! "ingest 全量失败" — `ingest::run` returning `Err` — short-circuits
//! because there are no new articles to process). The overall exit code
//! reflects the most severe stage outcome; failures are surfaced to the
//! user via the run summary, not silently dropped.
//!
//! §4.11 lines 362-368 carve-out: when `app.ai.enabled = false`, `run`
//! must **proactively skip** `ai-run` (emit one INFO log line, do NOT
//! return exit 78) and proceed straight to `publish` per the
//! `(ai=false, include_unscored)` truth table. This differs from the
//! standalone `ai-run` invocation which intentionally fails with exit 78
//! because the user explicitly requested an action that contradicts the
//! current configuration.

use std::{
    io::{self, Write},
    time::Instant,
};

use rss_ai_news_config as config;
use serde::Serialize;

use crate::{
    args::{AiRunArgs, Cli, IngestArgs, PublishArgs, RunArgs},
    commands::{ai_run, ingest, publish_all},
    context_factory::open_write_storage,
    error::CliError,
    exit_code::ExitCode,
    output::{CommandSummary, RenderedError},
};

/// Reason an ai-run stage was skipped (rather than executed-and-failed).
/// Currently the only producer is the §4.11 `ai.enabled=false` branch.
pub const AI_RUN_SKIP_REASON_DISABLED: &str = "ai.enabled=false";

#[derive(Debug, Clone, Serialize)]
pub struct StageFailure {
    pub stage: &'static str,
    pub error_kind: String,
    pub message: String,
    pub exit_code_value: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunCommandSummary {
    pub ingest: Option<ingest::IngestCommandSummary>,
    pub ai_run: Option<ai_run::AiRunCommandSummary>,
    pub publish: Option<publish_all::PublishAllCommandSummary>,
    pub stage_failures: Vec<StageFailure>,
    /// `Some(reason)` iff the ai-run stage was deliberately skipped per
    /// §4.11 (e.g. `ai.enabled=false`). Distinguishes the "skipped" branch
    /// from the "executed and failed" branch — both surface as
    /// `ai_run = None`, but only the latter pushes a `StageFailure`.
    /// `None` means the stage either ran (regardless of outcome) or was
    /// not reached because an earlier stage short-circuited.
    pub ai_run_skip_reason: Option<&'static str>,
    pub overall_duration_seconds: f64,
}

impl RunCommandSummary {
    /// Most severe exit code across all stage failures. `Success` if no
    /// failures were recorded. Severity ordering follows the numeric
    /// `ExitCode::as_i32()` value (higher = more severe), so a config
    /// failure (78) outranks a runtime failure (1).
    pub fn derive_exit_code(&self) -> ExitCode {
        self.stage_failures
            .iter()
            .map(|failure| failure.exit_code_value)
            .max()
            .map_or(ExitCode::Success, ExitCode::from_i32)
    }
}

impl CommandSummary for RunCommandSummary {
    fn exit_code(&self) -> ExitCode {
        self.derive_exit_code()
    }

    fn errors(&self) -> Vec<RenderedError> {
        self.stage_failures
            .iter()
            .map(|failure| RenderedError {
                kind: format!("{}_{}", failure.stage, failure.error_kind),
                message: format!("[{}] {}", failure.stage, failure.message),
            })
            .collect()
    }

    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        if self.stage_failures.is_empty() {
            writeln!(writer, "Run completed:")?;
        } else {
            writeln!(writer, "Run failed:")?;
        }
        match &self.ingest {
            Some(ingest) => writeln!(
                writer,
                "  Ingest entries inserted: {}",
                ingest.entries_inserted
            )?,
            None => writeln!(writer, "  Ingest:                  failed")?,
        }
        match (&self.ai_run, self.ai_run_skip_reason) {
            (Some(ai_run), _) => writeln!(
                writer,
                "  AI claimed:              {}",
                ai_run.process_claimed
            )?,
            (None, Some(reason)) => {
                writeln!(writer, "  AI:                      skipped ({reason})")?
            }
            (None, None) => writeln!(writer, "  AI:                      failed")?,
        }
        match &self.publish {
            Some(publish) => writeln!(
                writer,
                "  Publish records:         {}",
                publish.categories.len()
            )?,
            None => writeln!(writer, "  Publish:                 skipped or failed")?,
        }
        writeln!(
            writer,
            "  Duration:                {:.2}s",
            self.overall_duration_seconds
        )?;
        if !self.stage_failures.is_empty() {
            writeln!(writer, "Stage failures ({}):", self.stage_failures.len())?;
            for failure in &self.stage_failures {
                writeln!(
                    writer,
                    "  [{}] ({}) {}",
                    failure.stage, failure.error_kind, failure.message
                )?;
            }
        }
        Ok(())
    }
}

fn record_stage_failure(failures: &mut Vec<StageFailure>, stage: &'static str, error: &CliError) {
    failures.push(StageFailure {
        stage,
        error_kind: error.error_kind().to_string(),
        message: error.display_user(),
        exit_code_value: error.exit_code().as_i32(),
    });
}

/// Records the failures a stage reported through its summary (it returned
/// `Ok` but its own exit code is non-zero, e.g. publish store-local failed).
fn record_summary_failures<S: CommandSummary>(
    failures: &mut Vec<StageFailure>,
    stage: &'static str,
    summary: &S,
) {
    let exit_code = summary.exit_code();
    if exit_code == ExitCode::Success {
        return;
    }
    let errors = summary.errors();
    if errors.is_empty() {
        failures.push(StageFailure {
            stage,
            error_kind: "stage_failed".to_string(),
            message: format!("{stage} reported status {}", summary.status()),
            exit_code_value: exit_code.as_i32(),
        });
    }
    for error in errors {
        failures.push(StageFailure {
            stage,
            error_kind: error.kind,
            message: error.message,
            exit_code_value: exit_code.as_i32(),
        });
    }
}

pub async fn run(cli: &Cli, args: &RunArgs) -> Result<RunCommandSummary, CliError> {
    let started = Instant::now();

    // The config is loaded and the storage pool opened exactly once, so every
    // stage of one run sees the same configuration (no hot reload within a
    // process, docs/plan/13) and migrations / config-version checks run once.
    //
    // Only structural checks run here; environment checks belong to each
    // stage (ingest / publish preflight, per-category AI credentials), so one
    // category missing credentials is an ai-run stage failure (exit 78 in the
    // summary) instead of aborting ingest and publish for every category.
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())
        .map_err(CliError::Config)?;
    let ai_enabled = loaded.app.ai.enabled;

    // `run` 内部 ingest / ai-run 共用同一 --max-batches 生效值（已由
    // CliOverrides 写入 loaded.app.runtime.max_batches_per_run）。
    let ingest_args = IngestArgs {
        batch_size: args.ingest_batch_size.unwrap_or(50),
        max_batches: args.max_batches,
        ..IngestArgs::default()
    };
    let ai_args = AiRunArgs {
        batch_size: args.ai_batch_size.unwrap_or(20),
        model: None,
        max_batches: args.max_batches,
    };
    let publish_args = PublishArgs {
        date: args.publish_date.clone(),
        local_only: false,
        force: false,
    };

    let mut stage_failures: Vec<StageFailure> = Vec::new();
    let mut ai_run_skip_reason: Option<&'static str> = None;

    let pool = match ingest::preflight(&loaded) {
        Ok(()) => match open_write_storage(&loaded).await {
            Ok(pool) => Some(pool),
            Err(err) => {
                record_stage_failure(&mut stage_failures, "ingest", &err);
                None
            }
        },
        Err(err) => {
            record_stage_failure(&mut stage_failures, "ingest", &err);
            None
        }
    };
    let ingest_summary = match &pool {
        Some(pool) => match ingest::run_loaded(&loaded, pool, &ingest_args).await {
            Ok(summary) => {
                record_summary_failures(&mut stage_failures, "ingest", &summary);
                Some(summary)
            }
            Err(err) => {
                record_stage_failure(&mut stage_failures, "ingest", &err);
                None
            }
        },
        None => None,
    };

    // "全量失败导致无新文章": when ingest returns Err the database has no new
    // entries to process, so ai-run and publish would either no-op or fail on
    // stale state. Skip them and let the ingest failure be the sole cause.
    let (ai_run_summary, publish_summary) = match (&pool, &ingest_summary) {
        (Some(pool), Some(_)) => {
            let ai_run_summary = if !ai_enabled {
                // ai.enabled=false: skip ai-run with one INFO line and no
                // StageFailure (the standalone ai-run exit-78 contract does
                // not apply when run orchestrates it implicitly).
                tracing::info!(
                    stage = "ai-run",
                    reason = AI_RUN_SKIP_REASON_DISABLED,
                    "AI disabled (ai.enabled=false), skipping ai-run"
                );
                ai_run_skip_reason = Some(AI_RUN_SKIP_REASON_DISABLED);
                None
            } else {
                // Every selected category, each with its own credentials.
                let categories = loaded.categories_filtered().cloned().collect::<Vec<_>>();
                match ai_run::run_categories(&loaded, pool, &categories, &ai_args).await {
                    Ok(summary) => {
                        record_summary_failures(&mut stage_failures, "ai-run", &summary);
                        Some(summary)
                    }
                    Err(err) => {
                        record_stage_failure(&mut stage_failures, "ai-run", &err);
                        None
                    }
                }
            };
            let publish_result = match publish_all::preflight(&loaded, &publish_args) {
                Ok(()) => publish_all::run_loaded(&loaded, pool, &publish_args).await,
                Err(err) => Err(err),
            };
            let publish_summary = match publish_result {
                Ok(summary) => {
                    record_summary_failures(&mut stage_failures, "publish", &summary);
                    Some(summary)
                }
                Err(err) => {
                    record_stage_failure(&mut stage_failures, "publish", &err);
                    None
                }
            };
            (ai_run_summary, publish_summary)
        }
        _ => (None, None),
    };

    Ok(RunCommandSummary {
        ingest: ingest_summary,
        ai_run: ai_run_summary,
        publish: publish_summary,
        stage_failures,
        ai_run_skip_reason,
        overall_duration_seconds: started.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(stage: &'static str, kind: &str, exit_code_value: i32) -> StageFailure {
        StageFailure {
            stage,
            error_kind: kind.to_string(),
            message: format!("simulated {stage} failure"),
            exit_code_value,
        }
    }

    fn empty_summary() -> RunCommandSummary {
        RunCommandSummary {
            ingest: None,
            ai_run: None,
            publish: None,
            stage_failures: Vec::new(),
            ai_run_skip_reason: None,
            overall_duration_seconds: 0.0,
        }
    }

    #[test]
    fn no_failures_yields_success_status_and_exit_code() {
        let summary = empty_summary();
        assert_eq!(summary.status(), "success");
        assert!(summary.errors().is_empty());
        assert_eq!(summary.derive_exit_code().as_i32(), 0);
    }

    #[test]
    fn publish_failure_propagates_to_status_and_exit_code() {
        let mut summary = empty_summary();
        summary
            .stage_failures
            .push(failure("publish", "runtime", 1));
        assert_eq!(summary.status(), "fail");
        assert_eq!(summary.errors().len(), 1);
        assert_eq!(summary.derive_exit_code().as_i32(), 1);
    }

    #[test]
    fn failed_publish_all_summary_becomes_a_stage_failure() {
        use crate::commands::publish::{StageVerdict, stage};
        use crate::commands::publish_all::PublishAllCategorySummary;

        let publish = publish_all::PublishAllCommandSummary {
            date: "2026-05-18".to_string(),
            render_version: 1,
            mode: "local".to_string(),
            categories: vec![PublishAllCategorySummary {
                category: "ai".to_string(),
                publish_record_id: 1,
                items: 1,
                local_path: None,
                commit_sha: None,
                remote_target: None,
                stages: vec![
                    stage("init", "created", StageVerdict::Ok),
                    stage(
                        "store_local",
                        "Failed { error_kind: \"local_io\" }",
                        StageVerdict::Failed,
                    ),
                ],
            }],
            commit_sha: None,
            forced: false,
        };
        assert_eq!(publish.exit_code(), ExitCode::RuntimeError);

        let mut failures = Vec::new();
        record_summary_failures(&mut failures, "publish", &publish);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].stage, "publish");
        assert_eq!(failures[0].error_kind, "publish_store_local");
        assert_eq!(failures[0].exit_code_value, 1);

        let mut summary = empty_summary();
        summary.publish = Some(publish);
        summary.stage_failures = failures;
        assert_eq!(summary.exit_code(), ExitCode::RuntimeError);
        assert_eq!(summary.status(), "fail");
    }

    #[test]
    fn successful_child_summary_records_no_failure() {
        let mut failures = Vec::new();
        record_summary_failures(&mut failures, "ai-run", &empty_summary());
        assert!(failures.is_empty());
    }

    #[test]
    fn config_failure_outranks_runtime_failure() {
        let mut summary = empty_summary();
        summary.stage_failures.push(failure("ai-run", "runtime", 1));
        summary
            .stage_failures
            .push(failure("publish", "config", 78));
        assert_eq!(summary.derive_exit_code().as_i32(), 78);
    }

    #[test]
    fn ai_run_skipped_via_disabled_carve_out_is_not_a_failure() {
        // §4.11 lines 362-368: when run skips ai-run because
        // ai.enabled=false, the summary must report "success" overall and
        // exit 0 — the carve-out specifically rejects exit 78 here.
        let mut summary = empty_summary();
        summary.ai_run_skip_reason = Some(AI_RUN_SKIP_REASON_DISABLED);
        assert_eq!(summary.status(), "success");
        assert!(summary.errors().is_empty());
        assert_eq!(summary.derive_exit_code().as_i32(), 0);
    }

    #[test]
    fn ai_run_skipped_renders_distinct_pretty_line_from_failed() {
        // The pretty output must visibly distinguish "skipped" from
        // "failed" so operators can tell whether an action is required
        // (config mismatch) or whether the carve-out fired (expected).
        let mut skipped = empty_summary();
        skipped.ai_run_skip_reason = Some(AI_RUN_SKIP_REASON_DISABLED);
        let mut buf = Vec::new();
        skipped.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("skipped (ai.enabled=false)"),
            "pretty output missing skip reason: {text}"
        );

        let failed = empty_summary();
        let mut buf = Vec::new();
        failed.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("AI:                      failed"),
            "pretty output missing failed line: {text}"
        );
    }

    #[test]
    fn ai_run_skip_reason_serializes_in_json() {
        let mut summary = empty_summary();
        summary.ai_run_skip_reason = Some(AI_RUN_SKIP_REASON_DISABLED);
        let value = serde_json::to_value(&summary).expect("serialize summary");
        assert_eq!(value["ai_run_skip_reason"], "ai.enabled=false");
    }

    #[test]
    fn errors_carry_stage_prefix_for_json_consumers() {
        let mut summary = empty_summary();
        summary
            .stage_failures
            .push(failure("publish", "runtime", 1));
        let errors = summary.errors();
        assert_eq!(errors[0].kind, "publish_runtime");
        assert!(errors[0].message.starts_with("[publish] "));
    }
}
