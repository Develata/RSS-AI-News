//! Command interface: clap-based parser + output formatting.

pub mod args;
pub mod commands;
pub mod context_factory;
pub mod db_url;
pub mod error;
pub mod exit_code;
pub mod output;

use clap::Parser;

pub use exit_code::ExitCode;

use std::sync::Arc;

use crate::{
    args::Cli,
    commands::dispatch,
    output::{OutputFormat, OutputWriter},
};

fn spawn_metrics_server(raw_bind: &str) {
    let bind = raw_bind.trim();
    if bind.is_empty() {
        return;
    }
    let addr: std::net::SocketAddr = match bind.parse() {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!(
                "[observability] --metrics-bind {bind:?} 不是合法 SocketAddr: {error}; 跳过 metrics server"
            );
            return;
        }
    };
    let recorder = Arc::new(rss_ai_news_observability::PrometheusMetrics::new());
    tokio::spawn(async move {
        if let Err(error) = rss_ai_news_observability::serve_metrics(addr, recorder).await {
            eprintln!("[observability] metrics server exited: {error}");
        }
    });
}

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

    // F15-14 W9-F2: --metrics-bind 非空时启动 prometheus `/metrics` 后台
    // 服务。recorder Arc 移入 spawned task；task 在进程退出时随 tokio
    // runtime drop 而退出。CLI 自建 recorder 已就绪 + 暴露空 registry；
    // 业务侧 counter_inc / histogram_observe 接入是独立追踪项（与 task T901
    // metrics 注册解耦——T901 已勾完，业务 instrumentation 留 v0.2+ follow-up）。
    spawn_metrics_server(&cli.metrics_bind);

    let mut writer = OutputWriter::new(OutputFormat::from(cli.output_format));
    match dispatch(cli, &mut writer).await {
        Ok(exit) => exit,
        Err(error) => {
            let exit = error.exit_code();
            // W11-P4-fix2.H2 lint：emit_failure 返 io::Result（写 stderr），
            // 进程即将退出的错误路径，stderr 关闭等故障无救赎；显式 `.ok()`
            // 表达 "尽力 emit + 失败不阻断 exit"，比 `let _ =` 通过 lint。
            writer.emit_failure(error.command_name(), &error).ok();
            exit
        }
    }
}
