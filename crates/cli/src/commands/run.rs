//! `run` command — orchestrates ingest → ai-run → publish in one invocation.
//!
//! Per `docs/design/cli-semantics.md` §4.11: any single stage failing must
//! NOT block downstream stages (with the explicit exception that an
//! "ingest 全量失败" — `ingest::run` returning `Err` — short-circuits
//! because there are no new articles to process). The overall exit code
//! reflects the most severe stage outcome; failures are surfaced to the
//! user via the run summary, not silently dropped.

use std::{
    io::{self, Write},
    time::Instant,
};

use serde::Serialize;

use crate::{
    args::{AiRunArgs, Cli, IngestArgs, PublishArgs, RunArgs},
    commands::{ai_run, ingest, publish},
    error::CliError,
    exit_code::ExitCode,
    output::{CommandSummary, RenderedError},
};

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
    pub publish: Option<publish::PublishCommandSummary>,
    pub stage_failures: Vec<StageFailure>,
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
            .map(|max_value| match max_value {
                0 => ExitCode::Success,
                1 => ExitCode::RuntimeError,
                2 => ExitCode::UserError,
                78 => ExitCode::ConfigError,
                _ => ExitCode::RuntimeError,
            })
            .unwrap_or(ExitCode::Success)
    }
}

impl CommandSummary for RunCommandSummary {
    fn status(&self) -> &'static str {
        if self.stage_failures.is_empty() {
            "success"
        } else {
            "fail"
        }
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
        writeln!(writer, "Run completed:")?;
        match &self.ingest {
            Some(ingest) => writeln!(
                writer,
                "  Ingest entries inserted: {}",
                ingest.entries_inserted
            )?,
            None => writeln!(writer, "  Ingest:                  failed")?,
        }
        match &self.ai_run {
            Some(ai_run) => writeln!(
                writer,
                "  AI claimed:              {}",
                ai_run.process_claimed
            )?,
            None => writeln!(writer, "  AI:                      skipped or failed")?,
        }
        match &self.publish {
            Some(publish) => writeln!(
                writer,
                "  Publish record:          {}",
                publish.publish_record_id
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

fn record_stage_failure(
    failures: &mut Vec<StageFailure>,
    stage: &'static str,
    error: &CliError,
) {
    failures.push(StageFailure {
        stage,
        error_kind: error.error_kind().to_string(),
        message: error.display_user(),
        exit_code_value: error.exit_code().as_i32(),
    });
}

pub async fn run(cli: &Cli, args: &RunArgs) -> Result<RunCommandSummary, CliError> {
    let started = Instant::now();
    let ingest_args = IngestArgs {
        batch_size: args.ingest_batch_size.unwrap_or(50),
        ..IngestArgs::default()
    };
    let ai_args = AiRunArgs {
        batch_size: args.ai_batch_size.unwrap_or(20),
        model: None,
    };
    let publish_args = PublishArgs {
        date: args.publish_date.clone(),
        local_only: false,
        force: false,
    };

    let mut stage_failures: Vec<StageFailure> = Vec::new();

    let ingest_summary = match ingest::run(cli, &ingest_args).await {
        Ok(summary) => Some(summary),
        Err(err) => {
            record_stage_failure(&mut stage_failures, "ingest", &err);
            None
        }
    };

    // §4.11 line 360 carve-out: "全量失败导致无新文章" — when ingest
    // returns Err the database has no new entries to process, so ai-run
    // and publish would either no-op or fail on stale state. Skip them
    // and let the caller see the ingest failure as the sole cause.
    let (ai_run_summary, publish_summary) = if ingest_summary.is_none() {
        (None, None)
    } else {
        let ai_run_summary = match ai_run::run(cli, &ai_args).await {
            Ok(summary) => Some(summary),
            Err(err) => {
                record_stage_failure(&mut stage_failures, "ai-run", &err);
                None
            }
        };
        let publish_summary = match publish::run(cli, &publish_args).await {
            Ok(summary) => Some(summary),
            Err(err) => {
                record_stage_failure(&mut stage_failures, "publish", &err);
                None
            }
        };
        (ai_run_summary, publish_summary)
    };

    Ok(RunCommandSummary {
        ingest: ingest_summary,
        ai_run: ai_run_summary,
        publish: publish_summary,
        stage_failures,
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
    fn config_failure_outranks_runtime_failure() {
        let mut summary = empty_summary();
        summary.stage_failures.push(failure("ai-run", "runtime", 1));
        summary
            .stage_failures
            .push(failure("publish", "config", 78));
        assert_eq!(summary.derive_exit_code().as_i32(), 78);
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
