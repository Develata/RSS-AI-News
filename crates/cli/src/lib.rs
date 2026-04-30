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
    init_tracing(&cli.log_level, cli.log_format.as_str());

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

fn init_tracing(log_level: &str, log_format: &str) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));
    let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);
    if log_format == "json" {
        builder.json().init();
    } else {
        builder.init();
    }
}
