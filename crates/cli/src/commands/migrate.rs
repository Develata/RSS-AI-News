use std::io::{self, Write};

use rss_ai_news_config as config;
use rss_ai_news_storage::{build_sqlite_pool, run_migrations};
use serde::Serialize;

use crate::{args::Cli, error::CliError, output::CommandSummary};

#[derive(Debug, Clone, Serialize)]
pub struct MigrateCommandSummary {
    pub action: String,
    pub applied_versions: Vec<i64>,
    pub current_version: Option<i64>,
}

impl CommandSummary for MigrateCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Migrate {} completed:", self.action)?;
        writeln!(
            writer,
            "  Current version: {}",
            self.current_version
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        )?;
        writeln!(
            writer,
            "  Applied migrations: {}",
            self.applied_versions.len()
        )
    }
}

// migrate is an infrastructure command: it only opens SQLite and runs the
// embedded schema migrations. It must not be gated by OPENAI_* / RSSHUB_BASE_URL
// env presence — those are business-credential concerns enforced for AI / fetch
// commands by `validate::run_general_checks`. See loader::load_skip_env_checks.
pub async fn run(cli: &Cli) -> Result<MigrateCommandSummary, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    let pool = open_pool(&loaded.app).await?;
    run_migrations(&pool).await?;
    summary("run", &pool).await
}

pub async fn check(cli: &Cli) -> Result<MigrateCommandSummary, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    let pool = open_pool(&loaded.app).await?;
    summary("check", &pool).await
}

async fn open_pool(app: &config::AppConfig) -> Result<sqlx::SqlitePool, CliError> {
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    build_sqlite_pool(
        &app.database.sqlite_path,
        app.database.max_connections,
        busy_timeout_ms,
    )
    .await
    .map_err(CliError::Storage)
}

async fn summary(action: &str, pool: &sqlx::SqlitePool) -> Result<MigrateCommandSummary, CliError> {
    let applied_versions =
        match sqlx::query_scalar::<_, i64>("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
        {
            Ok(values) => values,
            Err(sqlx::Error::Database(error)) if error.message().contains("_sqlx_migrations") => {
                Vec::new()
            }
            Err(error) => return Err(rss_ai_news_storage::StorageError::from(error).into()),
        };
    let current_version = applied_versions.iter().copied().max();
    Ok(MigrateCommandSummary {
        action: action.to_string(),
        applied_versions,
        current_version,
    })
}
