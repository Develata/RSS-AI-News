use serde_json::Value;

use crate::error::CliError;

pub async fn run() -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "migrate run".to_string(),
    })
}

pub async fn check() -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "migrate check".to_string(),
    })
}
