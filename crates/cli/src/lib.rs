//! Command interface: clap-based parser + output formatting.

pub mod args;
pub mod commands;
pub mod context_factory;
pub mod error;
pub mod exit_code;
pub mod output;

use clap::Parser;

pub use exit_code::ExitCode;

use crate::{
    args::Cli,
    commands::dispatch,
    output::{OutputFormat, OutputWriter},
};

pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    rss_ai_news_observability::tracing_init::init(
        rss_ai_news_observability::tracing_init::InitOptions {
            log_level: cli.log_level.clone(),
            log_format: cli.log_format.as_str().to_string(),
            log_file: String::new(),
        },
    );

    let mut writer = OutputWriter::new(OutputFormat::from(cli.output_format));
    match dispatch(cli, &mut writer).await {
        Ok(()) => ExitCode::Success,
        Err(error) => {
            let exit = error.exit_code();
            let _ = writer.emit_failure(error.command_name(), &error);
            exit
        }
    }
}
