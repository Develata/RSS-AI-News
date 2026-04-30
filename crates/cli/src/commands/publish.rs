use serde_json::Value;

use crate::{args::PublishArgs, error::CliError};

pub async fn run(_args: &PublishArgs) -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "publish".to_string(),
    })
}
