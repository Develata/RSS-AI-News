use serde_json::Value;

use crate::{args::ReindexArgs, error::CliError};

pub async fn run(_args: &ReindexArgs) -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "reindex".to_string(),
    })
}
