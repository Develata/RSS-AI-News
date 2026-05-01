use std::io::{self, Write};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{ReindexFlow, ReindexOptions, ReindexTarget as RuntimeReindexTarget};
use serde::Serialize;

use crate::{
    args::{Cli, ReindexArgs, ReindexTarget},
    commands::backfill::sha256_hex,
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct ReindexCommandSummary {
    pub target: String,
    pub new_rule_version_id: i64,
    pub scanned: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub conflict_skipped: u32,
    pub archived: u32,
    pub errors: u32,
}

impl CommandSummary for ReindexCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Reindex completed:")?;
        writeln!(writer, "  Target:            {}", self.target)?;
        writeln!(writer, "  Rule version:      {}", self.new_rule_version_id)?;
        writeln!(writer, "  Scanned:           {}", self.scanned)?;
        writeln!(writer, "  Updated:           {}", self.updated)?;
        writeln!(writer, "  Unchanged:         {}", self.unchanged)?;
        writeln!(writer, "  Conflict skipped:  {}", self.conflict_skipped)?;
        writeln!(writer, "  Archived:          {}", self.archived)?;
        writeln!(writer, "  Errors:            {}", self.errors)
    }
}

pub async fn run(cli: &Cli, args: &ReindexArgs) -> Result<ReindexCommandSummary, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let (_pool, ctx) = build_run_context("reindex", &loaded).await?;
    let target = match args.target {
        ReindexTarget::LinkHash => RuntimeReindexTarget::LinkHash,
        ReindexTarget::ContentHash => RuntimeReindexTarget::ContentHash,
        ReindexTarget::Categories => RuntimeReindexTarget::Categories,
    };
    let target_str = match args.target {
        ReindexTarget::LinkHash => "link-hash",
        ReindexTarget::ContentHash => "content-hash",
        ReindexTarget::Categories => "categories",
    };
    let tag = format!(
        "reindex-{}-{}",
        target_str,
        time::OffsetDateTime::now_utc().unix_timestamp()
    );
    let summary = ReindexFlow::new(ctx)
        .run(ReindexOptions {
            target,
            batch_size: args.batch_size,
            categories,
            new_rule_version_tag: tag,
            new_rule_version_description: format!("manual reindex for {target_str}"),
            new_rule_version_sha256: sha256_hex(target_str.as_bytes()),
        })
        .await?;

    Ok(ReindexCommandSummary {
        target: target_str.to_string(),
        new_rule_version_id: summary.new_rule_version_id,
        scanned: summary.scanned,
        updated: summary.updated,
        unchanged: summary.unchanged,
        conflict_skipped: summary.conflict_skipped,
        archived: summary.archived,
        errors: summary.errors,
    })
}
