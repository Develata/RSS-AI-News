use std::io::{self, Write};

use rss_ai_news_runtime::{RecentEntriesOptions, RecentEntriesResult, RuntimeError};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    args::{Cli, RecentEntriesArgs},
    context_factory::build_recent_entries_flow,
    error::CliError,
    output::CommandSummary,
};

pub const COMMAND_NAME: &str = "recent-entries";

#[derive(Debug, Clone, Serialize)]
pub struct RecentEntriesCommandSummary {
    pub schema_version: u32,
    pub generated_at: String,
    pub category: String,
    pub discovered_after: String,
    pub published_after: Option<String>,
    pub limit: u32,
    pub truncated: bool,
    pub source_health_truncated: bool,
    pub source_health: Vec<RecentSourceHealthSummary>,
    pub entries: Vec<RecentEntrySummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentSourceHealthSummary {
    pub source_key: String,
    pub priority: i64,
    pub last_fetched_at: Option<String>,
    pub last_success_at: Option<String>,
    pub consecutive_failures: i64,
    pub last_error_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentEntrySummary {
    pub id: i64,
    pub source_key: String,
    pub source_priority: i64,
    pub title: String,
    pub url: String,
    pub published_at: Option<String>,
    pub discovered_at: String,
    pub state: String,
}

impl CommandSummary for RecentEntriesCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Recent entries for {}:", self.category)?;
        writeln!(writer, "  Discovered after: {}", self.discovered_after)?;
        if let Some(published_after) = &self.published_after {
            writeln!(writer, "  Published after: {published_after}")?;
        }
        writeln!(
            writer,
            "  Sources: {}{}",
            self.source_health.len(),
            if self.source_health_truncated {
                " (truncated)"
            } else {
                ""
            }
        )?;
        writeln!(
            writer,
            "  Entries: {}{}",
            self.entries.len(),
            if self.truncated { " (truncated)" } else { "" }
        )?;
        for entry in &self.entries {
            writeln!(
                writer,
                "  - [{}] {} — {}",
                entry.source_key, entry.title, entry.url
            )?;
        }
        Ok(())
    }
}

pub async fn run(
    cli: &Cli,
    args: &RecentEntriesArgs,
) -> Result<RecentEntriesCommandSummary, CliError> {
    let category = cli
        .category
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(CliError::RecentEntriesCategoryRequired)?
        .to_string();

    let flow = build_recent_entries_flow(cli)
        .await
        .map_err(|error| error.in_command(COMMAND_NAME))?;
    let result = flow
        .execute(RecentEntriesOptions {
            category_key: category,
            discovered_after: args.discovered_after,
            published_after: args.published_after,
            limit: args.limit,
        })
        .await
        .map_err(|error| CliError::Runtime(error).in_command(COMMAND_NAME))?;

    summary_from_result(result).map_err(|error| error.in_command(COMMAND_NAME))
}

fn summary_from_result(
    result: RecentEntriesResult,
) -> Result<RecentEntriesCommandSummary, CliError> {
    let source_health = result
        .source_health
        .into_iter()
        .map(|source| {
            Ok(RecentSourceHealthSummary {
                source_key: source.source_key,
                priority: source.priority,
                last_fetched_at: optional_rfc3339(source.last_fetched_at)?,
                last_success_at: optional_rfc3339(source.last_success_at)?,
                consecutive_failures: source.consecutive_failures,
                last_error_kind: source.last_error_kind,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    let entries = result
        .entries
        .into_iter()
        .map(|entry| {
            Ok(RecentEntrySummary {
                id: entry.id,
                source_key: entry.source_key,
                source_priority: entry.source_priority,
                title: entry.title,
                url: entry.url,
                published_at: optional_rfc3339(entry.published_at)?,
                discovered_at: rfc3339(entry.discovered_at)?,
                state: entry.state,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;

    Ok(RecentEntriesCommandSummary {
        schema_version: 2,
        generated_at: rfc3339(result.generated_at)?,
        category: result.category,
        discovered_after: rfc3339(result.discovered_after)?,
        published_after: optional_rfc3339(result.published_after)?,
        limit: result.limit,
        truncated: result.truncated,
        source_health_truncated: result.source_health_truncated,
        source_health,
        entries,
    })
}

fn optional_rfc3339(value: Option<OffsetDateTime>) -> Result<Option<String>, CliError> {
    value.map(rfc3339).transpose()
}

fn rfc3339(value: OffsetDateTime) -> Result<String, CliError> {
    value.format(&Rfc3339).map_err(|error| {
        CliError::Runtime(RuntimeError::Config(format!(
            "cannot format timestamp as RFC3339: {error}"
        )))
    })
}
