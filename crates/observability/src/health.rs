//! Generic doctor outcomes and check contract; concrete checks live in runtime.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "message", rename_all = "snake_case")]
pub enum CheckOutcome {
    Ok(String),
    Warn(String),
    Fail(String),
    Info(String),
}

impl CheckOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Ok(_) => "ok",
            Self::Warn(_) => "warn",
            Self::Fail(_) => "fail",
            Self::Info(_) => "info",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Ok(message) | Self::Warn(message) | Self::Fail(message) | Self::Info(message) => {
                message
            }
        }
    }
}

#[async_trait]
pub trait HealthCheck: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self) -> CheckOutcome;
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CheckReport {
    pub items: Vec<(String, CheckOutcome)>,
}

impl CheckReport {
    pub fn has_fail(&self) -> bool {
        self.items
            .iter()
            .any(|(_, outcome)| matches!(outcome, CheckOutcome::Fail(_)))
    }

    pub fn has_warn(&self) -> bool {
        self.items
            .iter()
            .any(|(_, outcome)| matches!(outcome, CheckOutcome::Warn(_)))
    }
}
