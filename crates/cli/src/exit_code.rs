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
