use rss_ai_news_domain::{
    Score0To100,
    dto::ai::{AiFilteredOutput, AiOutput},
};
use serde::Deserialize;

use crate::error::AiError;

#[derive(Debug, Clone)]
pub enum ParsedResponse {
    Output(AiOutput),
    Filtered(AiFilteredOutput),
}

#[derive(Deserialize)]
struct RawAiSchemaV1 {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    importance_score: Option<i64>,
    #[serde(default)]
    keep_decision: Option<bool>,
    #[serde(default)]
    filter_reason: Option<String>,
}

pub fn parse_response(article_ai_result_id: i64, raw: &str) -> Result<ParsedResponse, AiError> {
    let schema = parse_schema(raw)?;
    let keep_decision = schema
        .keep_decision
        .ok_or_else(|| missing_field("keep_decision"))?;

    if keep_decision {
        let summary = required_non_empty(schema.summary, "summary")?;
        let importance_score = schema
            .importance_score
            .ok_or_else(|| missing_field("importance_score"))?;
        let importance_score = score_from_i64(importance_score)?;

        Ok(ParsedResponse::Output(AiOutput {
            article_ai_result_id,
            summary,
            tags: schema.tags.unwrap_or_default(),
            importance_score,
            keep_decision,
            raw_response: raw.to_string(),
        }))
    } else {
        let reason = required_non_empty(schema.filter_reason, "filter_reason")?;

        Ok(ParsedResponse::Filtered(AiFilteredOutput {
            article_ai_result_id,
            reason,
            raw_response: raw.to_string(),
        }))
    }
}

fn parse_schema(raw: &str) -> Result<RawAiSchemaV1, AiError> {
    match serde_json::from_str(raw) {
        Ok(schema) => Ok(schema),
        Err(first_error) => {
            let Some(json) = extract_first_json_object(raw) else {
                return Err(AiError::InvalidJson(first_error.to_string()));
            };

            serde_json::from_str(json).map_err(|err| AiError::InvalidJson(err.to_string()))
        }
    }
}

fn required_non_empty(value: Option<String>, field: &str) -> Result<String, AiError> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(missing_field(field)),
    }
}

fn missing_field(field: &str) -> AiError {
    AiError::MissingField {
        field: field.to_string(),
    }
}

fn score_from_i64(value: i64) -> Result<Score0To100, AiError> {
    let value = u8::try_from(value).map_err(|_| AiError::InvalidFieldValue {
        field: "importance_score".to_string(),
        reason: "must be an integer in 0..=100".to_string(),
    })?;

    Score0To100::try_new(value).map_err(|err| AiError::InvalidFieldValue {
        field: "importance_score".to_string(),
        reason: err.to_string(),
    })
}

fn extract_first_json_object(raw: &str) -> Option<&str> {
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, byte) in raw.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' if start.is_some() => in_string = true,
            b'{' => {
                if start.is_none() {
                    start = Some(index);
                }
                depth += 1;
            }
            b'}' if start.is_some() => {
                depth -= 1;
                if depth == 0 {
                    return raw.get(start?..=index);
                }
            }
            _ => {}
        }
    }

    None
}
