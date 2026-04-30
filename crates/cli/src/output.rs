use std::io::{self, Write};

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
                    "status": "success",
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
