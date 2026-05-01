use std::io::{self, Write};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{BackfillAiOptions, BackfillExtractOptions, BackfillFlow, RuntimeError};
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::{
    args::{BackfillArgs, BackfillTarget, Cli},
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct BackfillCommandSummary {
    pub target: String,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub feed_entries_examined: u32,
    pub feed_entries_reset: u32,
    pub new_prompt_version_id: Option<i64>,
    pub articles_scanned: u32,
    pub ai_tasks_inserted: u32,
    pub ai_tasks_conflict: u32,
}

impl CommandSummary for BackfillCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Backfill completed:")?;
        writeln!(writer, "  Target:               {}", self.target)?;
        writeln!(
            writer,
            "  Feed entries examined: {}",
            self.feed_entries_examined
        )?;
        writeln!(
            writer,
            "  Feed entries reset:    {}",
            self.feed_entries_reset
        )?;
        if let Some(id) = self.new_prompt_version_id {
            writeln!(writer, "  New prompt version:    {id}")?;
        }
        writeln!(writer, "  Articles scanned:      {}", self.articles_scanned)?;
        writeln!(
            writer,
            "  AI tasks inserted:     {}",
            self.ai_tasks_inserted
        )?;
        writeln!(
            writer,
            "  AI task conflicts:     {}",
            self.ai_tasks_conflict
        )
    }
}

pub async fn run(cli: &Cli, args: &BackfillArgs) -> Result<BackfillCommandSummary, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let date_from = parse_date_start(args.date_from.as_deref())?;
    let date_to = parse_date_start(args.date_to.as_deref())?;
    let (_pool, ctx) = build_run_context("backfill", &loaded).await?;
    let flow = BackfillFlow::new(ctx.clone());

    match args.target {
        BackfillTarget::Extract => {
            let summary = flow
                .extract(BackfillExtractOptions { date_from, date_to })
                .await?;
            Ok(BackfillCommandSummary {
                target: "extract".to_string(),
                date_from: args.date_from.clone(),
                date_to: args.date_to.clone(),
                feed_entries_examined: summary.examined,
                feed_entries_reset: summary.reset,
                new_prompt_version_id: None,
                articles_scanned: 0,
                ai_tasks_inserted: 0,
                ai_tasks_conflict: 0,
            })
        }
        BackfillTarget::Ai => {
            let category = super::ai_run::select_category(cli, &categories)?;
            let prompt_template = category
                .ai_override
                .as_ref()
                .and_then(|override_| override_.prompt_template.clone())
                .unwrap_or_else(|| "Summarize the following article.".to_string());
            let output_schema_version = ctx
                .rule_version_repo
                .get_or_create("ai_output_schema", "v1", "AI v1 schema", "v1")
                .await?;
            let tag = format!("backfill-{}", OffsetDateTime::now_utc().unix_timestamp());
            let summary = flow
                .ai(BackfillAiOptions {
                    date_from,
                    date_to,
                    batch_size: args.batch_size,
                    new_prompt_version_tag: tag,
                    new_prompt_version_sha256: sha256_hex(prompt_template.as_bytes()),
                    new_prompt_version_description: "manual backfill via CLI".to_string(),
                    model_id: loaded.app.ai.model.clone(),
                    output_schema_version,
                })
                .await?;
            Ok(BackfillCommandSummary {
                target: "ai".to_string(),
                date_from: args.date_from.clone(),
                date_to: args.date_to.clone(),
                feed_entries_examined: 0,
                feed_entries_reset: 0,
                new_prompt_version_id: Some(summary.new_prompt_version_id),
                articles_scanned: summary.articles_scanned,
                ai_tasks_inserted: summary.ai_tasks_inserted,
                ai_tasks_conflict: summary.ai_tasks_conflict,
            })
        }
    }
}

pub fn parse_date_start(value: Option<&str>) -> Result<Option<OffsetDateTime>, CliError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let mut parts = value.split('-');
    let year = parts.next().and_then(|v| v.parse::<i32>().ok());
    let month = parts.next().and_then(|v| v.parse::<u8>().ok());
    let day = parts.next().and_then(|v| v.parse::<u8>().ok());
    if parts.next().is_some() {
        return Err(invalid_date(value));
    }
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return Err(invalid_date(value));
    };
    let month = Month::try_from(month).map_err(|_| invalid_date(value))?;
    let date = Date::from_calendar_date(year, month, day).map_err(|_| invalid_date(value))?;
    Ok(Some(
        PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc(),
    ))
}

fn invalid_date(value: &str) -> CliError {
    CliError::Runtime(RuntimeError::Config(format!(
        "invalid date {value:?}; expected YYYY-MM-DD"
    )))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}
