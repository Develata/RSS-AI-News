//! OpenAI-compatible client + prompt rendering + response parsing.

pub mod client;
pub mod error;
pub mod parser;
pub mod prompt;

pub use client::{
    AiClient, AiClientConfig, AiResponse, InvokeOptions, OpenAiCompatClient, TokenUsage,
};
pub use error::AiError;
pub use parser::{ParsedResponse, parse_response};
pub use prompt::{PromptInput, PromptRenderConfig, render_prompt};

pub use rss_ai_news_domain::dto::ai::{AiFilteredOutput, AiOutput, AiTask};
