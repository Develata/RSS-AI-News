use serde_json::Value;

use crate::{args::AiRunArgs, error::CliError};

pub async fn run(_args: &AiRunArgs) -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "ai-run".to_string(),
    })
}
