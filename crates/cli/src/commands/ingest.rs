use std::{
    io::{self, Write},
    time::Instant,
};

use rss_ai_news_config::{self as config, CategoryConfig, LoadedConfig};
use rss_ai_news_runtime::{
    ExtractFlow, ExtractOptions, ExtractSummary, IngestFlow, IngestOptions,
    extract_lease_budget_seconds, fit_extract_batch_to_lease,
};
use rss_ai_news_storage::StoragePool;
use serde::Serialize;

use crate::{
    args::{Cli, IngestArgs},
    context_factory::{build_extract_deps, build_ingest_deps, open_write_storage},
    error::CliError,
    exit_code::ExitCode,
    output::{CommandSummary, RenderedError},
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
    /// `error_kind` when claiming extract work failed (database unavailable,
    /// …); per-source and per-entry failures stay in the counters above.
    pub extract_claim_error: Option<String>,
    /// Sources that failed because of our own storage (entry inserts, source
    /// rows); included in `sources_failed`.
    pub sources_storage_failed: u32,
    /// Extract entries whose path hit a failure of our own storage (article
    /// write, summary read, release); a run failure like the source count.
    pub extract_storage_failed: u32,
    /// Extract releases that matched no row because another run reclaimed the
    /// lease; reported, not a run failure (that run owns the entry now).
    pub extract_lease_lost: u32,
    /// Extract did not run: not even one wave of `concurrent_fetches` fits
    /// `lease.fetch_duration_seconds` (R2-S9). A run failure (exit 1).
    pub extract_lease_too_short: bool,
    pub duration_seconds: f64,
}

impl IngestCommandSummary {
    /// Storage-level failures of the run. Remote feed / network / parse
    /// failures stay in the counters and are retried by the next run.
    fn infra_errors(&self) -> Vec<RenderedError> {
        let mut errors = Vec::new();
        if self.sources_storage_failed > 0 {
            errors.push(RenderedError {
                kind: "ingest_storage_failed".to_string(),
                message: format!(
                    "{} sources failed to persist entries",
                    self.sources_storage_failed
                ),
            });
        }
        if self.extract_storage_failed > 0 {
            errors.push(RenderedError {
                kind: "extract_storage_failed".to_string(),
                message: format!(
                    "{} extract entries hit a storage failure",
                    self.extract_storage_failed
                ),
            });
        }
        if self.extract_lease_too_short {
            errors.push(RenderedError {
                kind: "extract_lease_too_short".to_string(),
                message: "extract skipped: one wave of http.concurrent_fetches can outlive \
                          lease.fetch_duration_seconds; raise the lease or lower \
                          http.timeout_seconds"
                    .to_string(),
            });
        }
        if let Some(kind) = &self.extract_claim_error {
            errors.push(RenderedError {
                kind: format!("extract_claim_{kind}"),
                message: format!("claiming extract work failed ({kind})"),
            });
        }
        errors
    }
}

impl CommandSummary for IngestCommandSummary {
    fn exit_code(&self) -> ExitCode {
        if self.infra_errors().is_empty() {
            ExitCode::Success
        } else {
            ExitCode::RuntimeError
        }
    }

    fn errors(&self) -> Vec<RenderedError> {
        self.infra_errors()
    }

    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        let errors = self.infra_errors();
        if errors.is_empty() {
            writeln!(writer, "Ingest completed:")?;
        } else {
            writeln!(writer, "Ingest failed:")?;
            for error in &errors {
                writeln!(writer, "  ! {}", error.message)?;
            }
        }
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
        writeln!(
            writer,
            "  Extract lease lost:   {}",
            self.extract_lease_lost
        )?;
        writeln!(writer, "  Tasks panicked:       {}", self.tasks_panicked)?;
        writeln!(
            writer,
            "  Duration:             {:.2}s",
            self.duration_seconds
        )
    }
}

pub async fn run(cli: &Cli, args: &IngestArgs) -> Result<IngestCommandSummary, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    preflight(&loaded)?;
    let pool = open_write_storage(&loaded).await?;
    run_loaded(&loaded, &pool, args).await
}

/// Config checks for ingest; run before any storage is opened.
pub(crate) fn preflight(loaded: &LoadedConfig) -> Result<(), CliError> {
    config::validate::run_command_checks(
        loaded,
        config::validate::CommandKind::Ingest,
        &config::validate::CommandFlags::default(),
    )?;
    Ok(())
}

/// The extract batch size for this run: `requested`, shrunk to whole waves
/// when its estimated budget exceeds the lease (R3E-S9) (the batch size is a CLI argument,
/// so this is decided per run, like the AI lease budget). Shrinking instead of
/// failing keeps configs that ran before the budget existed running. `None`
/// when not even one wave fits (R2-S9): extract is skipped for the run.
fn extract_batch_size(loaded: &LoadedConfig, requested: u32) -> Option<u32> {
    let app = &loaded.app;
    let concurrent = app.http.concurrent_fetches;
    let timeout = app.http.timeout_seconds;
    let lease = app.lease.fetch_duration_seconds;
    let Some(fitted) = fit_extract_batch_to_lease(requested, concurrent, timeout, lease) else {
        tracing::error!(
            stage = "extract",
            one_wave_seconds = extract_lease_budget_seconds(1, concurrent, timeout),
            lease_seconds = lease,
            "extract skipped: one wave of concurrent_fetches can outlive \
             lease.fetch_duration_seconds; raise the lease or lower http.timeout_seconds"
        );
        return None;
    };
    if fitted < requested {
        tracing::warn!(
            stage = "extract",
            requested_batch_size = requested,
            batch_size = fitted,
            lease_seconds = lease,
            "extract batch shrunk so its estimated budget fits lease.fetch_duration_seconds \
             (ceil(batch / concurrent_fetches) x (http.timeout_seconds + margin))"
        );
    }
    Some(fitted)
}

/// Ingest (and extract unless `--skip-fetch`) on an already loaded config and
/// opened pool; callers run [`preflight`] first.
pub(crate) async fn run_loaded(
    loaded: &LoadedConfig,
    pool: &StoragePool,
    args: &IngestArgs,
) -> Result<IngestCommandSummary, CliError> {
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let started = Instant::now();
    let ctx = build_ingest_deps(loaded, pool)?;

    let ingest_flow =
        IngestFlow::with_source_secrets(ctx.clone(), categories, loaded.source_secrets.clone());
    let ingest_summary = ingest_flow.run(IngestOptions::default()).await;

    let batch_size = if args.skip_fetch {
        None
    } else {
        extract_batch_size(loaded, args.batch_size)
    };
    let extract_lease_too_short = !args.skip_fetch && batch_size.is_none();
    let extract_summary = if let Some(batch_size) = batch_size {
        let extract_flow =
            ExtractFlow::new(build_extract_deps(loaded, pool, ctx.run.clone()).await?);
        extract_flow
            .run(ExtractOptions {
                batch_size,
                max_attempts: loaded.app.retry.feed_entry_max_attempts,
                // F6-3: 从 app.runtime.max_batches_per_run 取生效值。
                // `--max-batches` 已经由 CliOverrides::apply_to_app
                // 覆盖到该字段（F5-6），所以此处只是把 CLI > config > 默认
                // 三层解析的结果直传到 flow。
                max_batches: loaded.app.runtime.max_batches_per_run,
                // `--category` scopes extract like it scopes ingest (plan/09 §2).
                category_key: loaded.cli_overrides.category_filter.clone(),
            })
            .await
    } else {
        ExtractSummary::default()
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
        extract_claim_error: extract_summary.claim_error,
        sources_storage_failed: ingest_summary.sources_storage_failed,
        extract_storage_failed: extract_summary.storage_failed,
        extract_lease_lost: extract_summary.lease_lost,
        extract_lease_too_short,
        duration_seconds: started.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> IngestCommandSummary {
        IngestCommandSummary {
            sources_attempted: 1,
            sources_succeeded: 1,
            sources_not_modified: 0,
            sources_failed: 0,
            entries_discovered: 1,
            entries_inserted: 1,
            articles_persisted: 0,
            articles_fallback: 0,
            fetch_failed: 0,
            tasks_panicked: 0,
            extract_claim_error: None,
            sources_storage_failed: 0,
            extract_storage_failed: 0,
            extract_lease_lost: 0,
            extract_lease_too_short: false,
            duration_seconds: 0.0,
        }
    }

    #[test]
    fn extract_storage_failures_fail_the_run_but_lost_leases_do_not() {
        let mut lost = summary();
        lost.extract_lease_lost = 2;
        assert_eq!(lost.exit_code(), ExitCode::Success);

        let mut failed = summary();
        failed.extract_storage_failed = 1;
        assert_eq!(failed.exit_code(), ExitCode::RuntimeError);
        assert_eq!(failed.errors()[0].kind, "extract_storage_failed");
    }
}
