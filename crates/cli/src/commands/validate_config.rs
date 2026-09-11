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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<config::validate::ConfigWarning>,
}

impl CommandSummary for ValidateConfigSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Config valid:")?;
        writeln!(writer, "  Config dir:     {}", self.config_dir)?;
        writeln!(writer, "  Categories:     {}", self.category_count)?;
        writeln!(writer, "  Sources:        {}", self.source_count)?;
        writeln!(writer, "  Config SHA-256: {}", self.config_sha256)?;
        for warning in &self.warnings {
            writeln!(
                writer,
                "  Warning [{}] {}: {}",
                warning.code, warning.field, warning.message
            )?;
        }
        Ok(())
    }
}

pub async fn run(cli: &Cli) -> Result<ValidateConfigSummary, CliError> {
    let mut overrides = cli.to_cli_overrides();
    // Logging has already been initialized from CLI flags. Inspect the actual
    // TOML values here, before the CLI defaults can hide ineffective settings.
    overrides.log_level = None;
    overrides.log_format = None;
    let loaded = config::load(&cli.config_dir, None, overrides)?;
    // W14-B：诊断命令全量审计每个板块的 AI 凭证可解析性（普通 load 只按
    // filtered 范围做全局 gate + ai-run 选定板块后 fail-fast，不查未选板块）。
    config::audit_ai_credentials(&loaded)?;
    Ok(ValidateConfigSummary {
        config_dir: cli.config_dir.display().to_string(),
        category_count: loaded.categories.len() as u32,
        source_count: loaded
            .categories
            .iter()
            .map(|category| category.sources.len() as u32)
            .sum(),
        warnings: config::validate::inert_config_warnings(&loaded.app),
        config_sha256: loaded.config_sha256,
    })
}
