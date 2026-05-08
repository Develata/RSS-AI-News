use std::{
    io::{self, Write},
    time::Instant,
};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{AiRunFlow, AiRunOptions, RuntimeError};
use serde::Serialize;

use crate::{
    args::{AiRunArgs, Cli},
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct AiRunCommandSummary {
    pub task_gen_scanned: u32,
    pub task_gen_inserted: u32,
    pub task_gen_conflict_skipped: u32,
    pub process_claimed: u32,
    pub process_succeeded: u32,
    pub process_filtered: u32,
    pub process_retryable_failed: u32,
    pub process_permanent_failed: u32,
    pub duration_seconds: f64,
}

impl CommandSummary for AiRunCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "AI run completed:")?;
        writeln!(writer, "  Task-gen scanned:      {}", self.task_gen_scanned)?;
        writeln!(
            writer,
            "  Task-gen inserted:     {}",
            self.task_gen_inserted
        )?;
        writeln!(
            writer,
            "  Task-gen conflicts:    {}",
            self.task_gen_conflict_skipped
        )?;
        writeln!(writer, "  Claimed:               {}", self.process_claimed)?;
        writeln!(
            writer,
            "  Succeeded:             {}",
            self.process_succeeded
        )?;
        writeln!(writer, "  Filtered:              {}", self.process_filtered)?;
        writeln!(
            writer,
            "  Failed (retryable):    {}",
            self.process_retryable_failed
        )?;
        writeln!(
            writer,
            "  Failed (permanent):    {}",
            self.process_permanent_failed
        )?;
        writeln!(
            writer,
            "  Duration:              {:.2}s",
            self.duration_seconds
        )
    }
}

pub async fn run(cli: &Cli, args: &AiRunArgs) -> Result<AiRunCommandSummary, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let category = select_category(cli, &categories)?;
    let started = Instant::now();
    let (_pool, ctx) = build_run_context("ai-run", &loaded).await?;

    let prompt_version = ctx
        .rule_version_repo
        .get_or_create("prompt", "default", "default prompt version", "0")
        .await?;
    let output_schema_version = ctx
        .rule_version_repo
        .get_or_create("ai_output_schema", "v1", "AI v1 schema", "v1")
        .await?;
    let model_id = args
        .model
        .clone()
        .or_else(|| {
            category
                .ai_override
                .as_ref()
                .and_then(|override_| override_.model.clone())
        })
        .unwrap_or_else(|| loaded.app.ai.model.clone());
    let prompt_template = category
        .ai_override
        .as_ref()
        .and_then(|override_| override_.prompt_template.clone())
        .unwrap_or_else(|| "Summarize the following article.".to_string());
    // Threshold inheritance must match the publish stage exactly: F4 audit
    // found that an ad-hoc `unwrap_or(50)` here diverged from the global
    // default (30) used by `effective_for_category`. Routing through a single
    // truth source guarantees AI filtering and publish selection see the
    // same `min_importance_score`. See docs/design/config-schema.md §4.5.
    let effective = loaded
        .effective_for_category(&category.category.key)
        .ok_or_else(|| {
            CliError::Runtime(RuntimeError::Config(format!(
                "category {} not found in loaded config",
                category.category.key
            )))
        })?;
    let min_importance_score = i32::from(effective.min_importance_score.get());
    let max_input_chars = category
        .ai_override
        .as_ref()
        .and_then(|override_| override_.max_input_chars)
        .unwrap_or(loaded.app.ai.max_input_chars);

    let summary = AiRunFlow::new(ctx)
        .run(AiRunOptions {
            task_gen_batch_size: args.batch_size,
            process_batch_size: args.batch_size,
            max_attempts: loaded.app.retry.ai_max_attempts,
            prompt_template,
            model_id,
            max_input_chars,
            max_tokens: loaded.app.ai.max_tokens,
            temperature: loaded.app.ai.temperature,
            min_importance_score,
            category_key: category.category.key.clone(),
            prompt_version,
            output_schema_version,
        })
        .await;

    Ok(AiRunCommandSummary {
        task_gen_scanned: summary.task_gen.scanned,
        task_gen_inserted: summary.task_gen.inserted,
        task_gen_conflict_skipped: summary.task_gen.conflict_skipped,
        process_claimed: summary.process.claimed,
        process_succeeded: summary.process.succeeded,
        process_filtered: summary.process.filtered,
        process_retryable_failed: summary.process.retryable_failed,
        process_permanent_failed: summary.process.permanent_failed,
        duration_seconds: started.elapsed().as_secs_f64(),
    })
}

pub(crate) fn select_category<'a>(
    cli: &Cli,
    categories: &'a [CategoryConfig],
) -> Result<&'a CategoryConfig, CliError> {
    if let Some(key) = &cli.category {
        return categories
            .iter()
            .find(|cat| cat.category.key == *key)
            .ok_or_else(|| {
                CliError::Runtime(rss_ai_news_runtime::RuntimeError::Config(format!(
                    "category {key} not found"
                )))
            });
    }
    if categories.len() == 1 {
        return Ok(&categories[0]);
    }
    Err(CliError::Runtime(
        rss_ai_news_runtime::RuntimeError::Config(
            "category is required when multiple or zero categories are configured".to_string(),
        ),
    ))
}
