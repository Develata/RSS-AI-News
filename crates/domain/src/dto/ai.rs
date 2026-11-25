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
    pub prompt_template: std::sync::Arc<str>,
    pub model_id: String,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Sent as `reasoning_effort` only when `Some`; `None` leaves the key out
    /// of the request body, so the provider's own default applies.
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// The OpenAI Chat Completions `reasoning_effort` level. The set is the union
/// accepted by OpenAI, DeepSeek and the new-api gateway (which maps the levels a
/// provider lacks onto its nearest one); providers without reasoning may reject
/// the field outright, which is why it is opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    /// The wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parses a wire value exactly (lowercase, no trimming).
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|effort| effort.as_str() == value)
    }
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
