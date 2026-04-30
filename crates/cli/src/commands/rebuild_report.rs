use serde_json::Value;

use crate::{args::RebuildReportArgs, error::CliError};

pub async fn run(_args: &RebuildReportArgs) -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "rebuild-report".to_string(),
    })
}
