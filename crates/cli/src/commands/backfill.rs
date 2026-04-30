use serde_json::Value;

use crate::{args::BackfillArgs, error::CliError};

pub async fn run(_args: &BackfillArgs) -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "backfill".to_string(),
    })
}
