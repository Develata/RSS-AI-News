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
    // F15-13 W9-F1: 持有 tracing-appender 的 WorkerGuard 到 run() 结束，
    // 让 non-blocking writer 在进程退出前 flush 完所有日志。--log-file 为
    // 空时 init() 返 None（stderr 模式），_guard 是 None 也无副作用。
    let _guard = rss_ai_news_observability::tracing_init::init(
        rss_ai_news_observability::tracing_init::InitOptions {
            log_level: cli.log_level.clone(),
            log_format: cli.log_format.as_str().to_string(),
            log_file: cli.log_file.clone(),
        },
    );

    let mut writer = OutputWriter::new(OutputFormat::from(cli.output_format));
    match dispatch(cli, &mut writer).await {
        Ok(exit) => exit,
        Err(error) => {
            let exit = error.exit_code();
            let _ = writer.emit_failure(error.command_name(), &error);
            exit
        }
    }
}
