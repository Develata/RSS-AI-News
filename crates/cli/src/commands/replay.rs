use serde_json::Value;

use crate::{args::ReplayArgs, error::CliError};

pub async fn run(_args: &ReplayArgs) -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "replay".to_string(),
    })
}
