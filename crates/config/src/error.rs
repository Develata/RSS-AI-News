//! Configuration errors and validation diagnostics.

use std::fmt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {path}")]
    FileNotFound { path: String },

    #[error("config parse failed in {path}: {reason}")]
    ParseFailed { path: String, reason: String },

    #[error("config validation failed:\n{report}")]
    ValidationFailed { report: DiagnosticReport },

    #[error("config version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: String, got: String },

    #[error("ai-run cannot be used when app.ai.enabled=false")]
    AiRunWhileDisabled,

    #[error("missing GitHub credentials for remote publish")]
    MissingGithubCredentials,
}

impl ConfigError {
    pub fn errors(&self) -> Vec<String> {
        match self {
            Self::ValidationFailed { report } => report
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.clone())
                .collect(),
            _ => vec![self.to_string()],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticReport {
    pub diagnostics: Vec<Diagnostic>,
}

impl DiagnosticReport {
    pub fn new(diagnostics: Vec<Diagnostic>) -> Self {
        Self { diagnostics }
    }

    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub fn extend(&mut self, other: DiagnosticReport) {
        self.diagnostics.extend(other.diagnostics);
    }
}

impl fmt::Display for DiagnosticReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Configuration error:")?;
        for diagnostic in &self.diagnostics {
            writeln!(
                f,
                "  [{}] {}: {}",
                diagnostic.source_file, diagnostic.field_path, diagnostic.message
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub source_file: String,
    pub field_path: String,
    pub message: String,
}

impl Diagnostic {
    pub fn new(
        source_file: impl Into<String>,
        field_path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source_file: source_file.into(),
            field_path: field_path.into(),
            message: message.into(),
        }
    }
}
