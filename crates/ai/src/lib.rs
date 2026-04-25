//! OpenAI-compatible client + prompt rendering + response parsing.
//! See docs/design/internal-dto-contracts.md § AI stage.

// TODO Phase 1 (deferred — Phase 1 is minimum ingest closure):
// - mod error;        // AiError (ClassifiedError impl, retryable vs permanent)
// - mod client;       // async-openai wrapper
// - mod prompt;       // category prompt template rendering
// - mod parser;       // structured response → AiOutput / AiFilteredOutput
// - mod rate_limit;   // per-model RPS / token budget
