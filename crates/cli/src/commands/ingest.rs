use std::{
    io::{self, Write},
    time::Instant,
};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{ExtractFlow, ExtractOptions, ExtractSummary, IngestFlow, IngestOptions};
use serde::Serialize;

use crate::{
    args::{Cli, IngestArgs},
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct IngestCommandSummary {
    pub sources_attempted: u32,
    pub sources_succeeded: u32,
    pub sources_not_modified: u32,
    pub sources_failed: u32,
    pub entries_discovered: u32,
    pub entries_inserted: u32,
    pub articles_persisted: u32,
    pub articles_fallback: u32,
    pub fetch_failed: u32,
    /// 因 task panic / cancel 而失败的任务数（ingest source + extract entry 之和；
    /// codex P2-1）。与业务失败计数分列，避免 panic 在运维输出中报 0 failure。
    pub tasks_panicked: u32,
    pub duration_seconds: f64,
}

impl CommandSummary for IngestCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Ingest completed:")?;
        writeln!(writer, "  Sources attempted:    {}", self.sources_attempted)?;
        writeln!(writer, "  Sources succeeded:    {}", self.sources_succeeded)?;
        writeln!(
            writer,
            "  Sources not-modified: {}",
            self.sources_not_modified
        )?;
        writeln!(writer, "  Sources failed:       {}", self.sources_failed)?;
        writeln!(
            writer,
            "  Entries discovered:   {}",
            self.entries_discovered
        )?;
        writeln!(writer, "  Entries inserted:     {}", self.entries_inserted)?;
        writeln!(
            writer,
            "  Articles persisted:   {}",
            self.articles_persisted
        )?;
        writeln!(writer, "  Articles fallback:    {}", self.articles_fallback)?;
        writeln!(writer, "  Fetch failed:         {}", self.fetch_failed)?;
        writeln!(writer, "  Tasks panicked:       {}", self.tasks_panicked)?;
        writeln!(
            writer,
            "  Duration:             {:.2}s",
            self.duration_seconds
        )
    }
}

pub async fn run(cli: &Cli, args: &IngestArgs) -> Result<IngestCommandSummary, CliError> {
    if cli.dry_run {
        return Err(CliError::DryRunNotImplemented);
    }
    if args.source.is_some() {
        return Err(CliError::IngestSourceFilterNotImplemented);
    }

    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let started = Instant::now();
    let ctx = build_run_context("ingest", &loaded, None).await?;

    let ingest_flow =
        IngestFlow::with_source_secrets(ctx.clone(), categories, loaded.source_secrets.clone());
    let ingest_summary = ingest_flow.run(IngestOptions::default()).await;

    let extract_summary = if args.skip_fetch {
        ExtractSummary::default()
    } else {
        let extract_flow = ExtractFlow::new(ctx.clone());
        extract_flow
            .run(ExtractOptions {
                batch_size: args.batch_size,
                max_attempts: ctx.app.retry.feed_entry_max_attempts,
                // F6-3: 从 app.runtime.max_batches_per_run 取生效值。
                // `--max-batches` 已经由 CliOverrides::apply_to_app
                // 覆盖到该字段（F5-6），所以此处只是把 CLI > config > 默认
                // 三层解析的结果直传到 flow。
                max_batches: ctx.app.runtime.max_batches_per_run,
            })
            .await
    };

    Ok(IngestCommandSummary {
        sources_attempted: ingest_summary.sources_attempted,
        sources_succeeded: ingest_summary.sources_succeeded,
        sources_not_modified: ingest_summary.sources_not_modified,
        sources_failed: ingest_summary.sources_failed,
        entries_discovered: ingest_summary.entries_discovered,
        entries_inserted: ingest_summary.entries_inserted,
        articles_persisted: extract_summary.persisted,
        articles_fallback: extract_summary.fallback_persisted,
        fetch_failed: extract_summary.permanent_failed + extract_summary.retryable_failed,
        tasks_panicked: ingest_summary.tasks_panicked + extract_summary.tasks_panicked,
        duration_seconds: started.elapsed().as_secs_f64(),
    })
}
