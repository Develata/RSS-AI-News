//! AI-stage DTOs.

use crate::Score0To100;

/// Task from runtime to ai crate: process an article.
#[derive(Debug, Clone)]
pub struct AiTask {
    pub article_ai_result_id: i64,
    pub article_id: i64,
    pub title: String,
    pub body_text: String,
    pub category_key: String,
    pub prompt_template: String,
    pub model_id: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

/// Successful AI output.
#[derive(Debug, Clone)]
pub struct AiOutput {
    pub article_ai_result_id: i64,
    pub summary: String,
    pub tags: Vec<String>,
    pub importance_score: Score0To100,
    pub keep_decision: bool,
    pub raw_response: String,
}

/// AI filtered output (article deemed not worth publishing).
#[derive(Debug, Clone)]
pub struct AiFilteredOutput {
    pub article_ai_result_id: i64,
    pub reason: String,
    pub raw_response: String,
}
