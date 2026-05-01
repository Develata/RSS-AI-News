use std::io::{self, Write};

use rss_ai_news_observability::health::CheckReport;
use rss_ai_news_runtime::doctor::deep_scan::DeepScanReport;
use serde::Serialize;
use serde_json::json;

use crate::{args, error::CliError};

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

pub trait CommandSummary: Serialize {
    fn status(&self) -> &'static str {
        "success"
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
                let envelope = json!({
                    "command": command,
                    "status": summary.status(),
                    "summary": summary,
                    "errors": [],
                });
                serde_json::to_writer(&mut handle, &envelope)?;
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
                let envelope = json!({
                    "command": command,
                    "status": "error",
                    "summary": null,
                    "errors": [{
                        "kind": error.error_kind(),
                        "message": error.display_user(),
                    }],
                });
                serde_json::to_writer(&mut handle, &envelope)?;
                writeln!(handle)
            }
        }
    }
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
