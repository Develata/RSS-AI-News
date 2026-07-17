use std::io::{self, Write};

use rss_ai_news_observability::health::CheckReport;
use rss_ai_news_runtime::doctor::deep_scan::DeepScanReport;
use serde::Serialize;
use serde_json::json;

use crate::{args, error::CliError};

pub use crate::commands::ai_run::AiRunCommandSummary;
pub use crate::commands::backfill::BackfillCommandSummary;
pub use crate::commands::migrate::MigrateCommandSummary;
pub use crate::commands::publish::{PublishCommandSummary, PublishStageOutcome};
pub use crate::commands::rebuild_report::RebuildReportCommandSummary;
pub use crate::commands::recent_entries::RecentEntriesCommandSummary;
pub use crate::commands::reindex::ReindexCommandSummary;
pub use crate::commands::replay::ReplayCommandSummary;
pub use crate::commands::run::RunCommandSummary;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Pretty,
    Json,
}

impl From<args::OutputFormat> for OutputFormat {
    fn from(value: args::OutputFormat) -> Self {
        match value {
            args::OutputFormat::Pretty => Self::Pretty,
            args::OutputFormat::Json => Self::Json,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderedError {
    pub kind: String,
    pub message: String,
}

pub trait CommandSummary: Serialize {
    fn status(&self) -> &'static str {
        "success"
    }

    /// Errors to surface in the JSON envelope's `errors` array. Default is
    /// empty; commands that aggregate stage-level failures (e.g. `run`)
    /// override this so JSON consumers see the failure list alongside the
    /// summary.
    fn errors(&self) -> Vec<RenderedError> {
        Vec::new()
    }

    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()>;
}

pub struct OutputWriter {
    format: OutputFormat,
}

impl OutputWriter {
    pub fn new(format: OutputFormat) -> Self {
        Self { format }
    }

    pub fn emit_success<S: CommandSummary>(
        &mut self,
        command: &str,
        summary: &S,
    ) -> io::Result<()> {
        match self.format {
            OutputFormat::Pretty => {
                let stdout = io::stdout();
                let mut handle = stdout.lock();
                summary.render_pretty(&mut handle)
            }
            OutputFormat::Json => {
                let stdout = io::stdout();
                let mut handle = stdout.lock();
                serde_json::to_writer(&mut handle, &success_envelope(command, summary))?;
                writeln!(handle)
            }
        }
    }

    pub fn emit_failure(&mut self, command: &str, error: &CliError) -> io::Result<()> {
        match self.format {
            OutputFormat::Pretty => {
                let stderr = io::stderr();
                let mut handle = stderr.lock();
                writeln!(handle, "{}", error.display_user())
            }
            OutputFormat::Json => {
                let stdout = io::stdout();
                let mut handle = stdout.lock();
                serde_json::to_writer(&mut handle, &failure_envelope(command, error))?;
                writeln!(handle)
            }
        }
    }
}

pub fn success_envelope<S: CommandSummary>(command: &str, summary: &S) -> serde_json::Value {
    json!({
        "command": command,
        "status": summary.status(),
        "summary": summary,
        "errors": summary.errors(),
    })
}

pub fn failure_envelope(command: &str, error: &CliError) -> serde_json::Value {
    json!({
        "command": command,
        "status": "error",
        "summary": null,
        "errors": [{
            "kind": error.error_kind(),
            "message": error.display_user(),
        }],
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCommandSummary {
    pub shallow_checks: Vec<DoctorCheckSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deep_scan: Option<Vec<DoctorInvariantSummary>>,
}

impl DoctorCommandSummary {
    pub fn new(report: CheckReport, deep_scan: Option<DeepScanReport>) -> Self {
        let shallow_checks = report
            .items
            .into_iter()
            .map(|(name, outcome)| DoctorCheckSummary {
                name,
                outcome: outcome.status().to_string(),
                message: outcome.message().to_string(),
            })
            .collect();
        let deep_scan = deep_scan.map(|report| {
            report
                .results
                .into_iter()
                .map(|result| DoctorInvariantSummary {
                    id: result.id.as_str().to_string(),
                    description: result.id.description().to_string(),
                    violations: result.total_count,
                    examples: result
                        .violations
                        .into_iter()
                        .map(|row| row.message)
                        .collect(),
                })
                .collect()
        });
        Self {
            shallow_checks,
            deep_scan,
        }
    }

    pub fn has_fail(&self) -> bool {
        self.shallow_checks
            .iter()
            .any(|item| item.outcome == "fail")
            || self
                .deep_scan
                .as_ref()
                .is_some_and(|items| items.iter().any(|item| item.violations > 0))
    }

    pub fn has_warn(&self) -> bool {
        self.shallow_checks
            .iter()
            .any(|item| item.outcome == "warn")
    }
}

impl CommandSummary for DoctorCommandSummary {
    fn status(&self) -> &'static str {
        if self.has_fail() {
            "fail"
        } else if self.has_warn() {
            "warn"
        } else {
            "ok"
        }
    }

    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        for item in &self.shallow_checks {
            writeln!(
                writer,
                "[{:<4}] {} {}",
                item.outcome.to_ascii_uppercase(),
                item.name,
                item.message
            )?;
        }
        if let Some(deep_scan) = &self.deep_scan {
            writeln!(writer, "--- deep scan ---")?;
            for item in deep_scan {
                if item.violations == 0 {
                    writeln!(writer, "[OK  ] {} {}", item.id, item.description)?;
                } else {
                    writeln!(
                        writer,
                        "[FAIL] {} {}  ({} violating rows)",
                        item.id, item.description, item.violations
                    )?;
                    for example in item.examples.iter().take(3) {
                        writeln!(writer, "        {example}")?;
                    }
                    if item.examples.len() > 3 {
                        writeln!(writer, "        ({} more)", item.examples.len() - 3)?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheckSummary {
    pub name: String,
    pub outcome: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorInvariantSummary {
    pub id: String,
    pub description: String,
    pub violations: u64,
    pub examples: Vec<String>,
}
