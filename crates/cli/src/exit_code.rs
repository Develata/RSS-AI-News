use crate::error::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Success,
    RuntimeError,
    UserError,
    ConfigError,
}

impl ExitCode {
    pub fn into_process_exit(self) -> std::process::ExitCode {
        std::process::ExitCode::from(match self {
            Self::Success => 0,
            Self::RuntimeError => 1,
            Self::UserError => 2,
            Self::ConfigError => 78,
        })
    }

    /// Inverse of [`Self::as_i32`]; unknown non-zero values map to
    /// `RuntimeError`.
    pub fn from_i32(value: i32) -> Self {
        match value {
            0 => Self::Success,
            2 => Self::UserError,
            78 => Self::ConfigError,
            _ => Self::RuntimeError,
        }
    }

    pub fn as_i32(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::RuntimeError => 1,
            Self::UserError => 2,
            Self::ConfigError => 78,
        }
    }
}

impl From<&CliError> for ExitCode {
    fn from(value: &CliError) -> Self {
        value.exit_code()
    }
}

impl From<CliError> for ExitCode {
    fn from(value: CliError) -> Self {
        value.exit_code()
    }
}
