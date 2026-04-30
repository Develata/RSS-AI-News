use serde_json::Value;

use crate::{args::RunArgs, error::CliError};

pub async fn run(_args: &RunArgs) -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "run".to_string(),
    })
}
