use serde_json::Value;

use crate::{args::DoctorArgs, error::CliError};

pub async fn run(_args: &DoctorArgs) -> Result<Value, CliError> {
    Err(CliError::NotImplementedYet {
        feature: "doctor".to_string(),
    })
}
