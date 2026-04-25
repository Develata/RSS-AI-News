//! rss-ai-news binary entry point.
//!
//! Thin shell: initialize tracing, delegate to cli::run().

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // TODO Phase 1: observability::tracing_init::init();
    rss_ai_news_cli::run().await
}
