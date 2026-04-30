use std::io::{self, Write};

use rss_ai_news_config as config;
use serde::Serialize;

use crate::{args::Cli, error::CliError, output::CommandSummary};

#[derive(Debug, Clone, Serialize)]
pub struct ValidateConfigSummary {
    pub config_dir: String,
    pub category_count: u32,
    pub source_count: u32,
    pub config_sha256: String,
}

impl CommandSummary for ValidateConfigSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Config valid:")?;
        writeln!(writer, "  Config dir:     {}", self.config_dir)?;
        writeln!(writer, "  Categories:     {}", self.category_count)?;
        writeln!(writer, "  Sources:        {}", self.source_count)?;
        writeln!(writer, "  Config SHA-256: {}", self.config_sha256)
    }
}

pub async fn run(cli: &Cli) -> Result<ValidateConfigSummary, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    Ok(ValidateConfigSummary {
        config_dir: cli.config_dir.display().to_string(),
        category_count: loaded.categories.len() as u32,
        source_count: loaded
            .categories
            .iter()
            .map(|category| category.sources.len() as u32)
            .sum(),
        config_sha256: loaded.config_sha256,
    })
}
