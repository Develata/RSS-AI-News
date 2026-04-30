use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    rss_ai_news_cli::run().await.into_process_exit()
}
