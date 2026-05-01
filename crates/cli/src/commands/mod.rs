use crate::{
    args::{Cli, Command, MigrateAction},
    error::CliError,
    output::OutputWriter,
};

pub mod ai_run;
pub mod backfill;
pub mod doctor;
pub mod ingest;
pub mod migrate;
pub mod publish;
pub mod rebuild_report;
pub mod reindex;
pub mod replay;
pub mod run;
pub mod validate_config;

pub async fn dispatch(cli: Cli, writer: &mut OutputWriter) -> Result<(), CliError> {
    match &cli.command {
        Command::ValidateConfig => {
            let summary = validate_config::run(&cli).await?;
            writer
                .emit_success("validate-config", &summary)
                .map_err(CliError::Io)?;
        }
        Command::Ingest(args) => {
            let summary = ingest::run(&cli, args).await?;
            writer
                .emit_success("ingest", &summary)
                .map_err(CliError::Io)?;
        }
        Command::AiRun(args) => {
            let summary = ai_run::run(&cli, args).await?;
            writer
                .emit_success("ai-run", &summary)
                .map_err(CliError::Io)?;
        }
        Command::Publish(args) => {
            let summary = publish::run(&cli, args).await?;
            writer
                .emit_success("publish", &summary)
                .map_err(CliError::Io)?;
        }
        Command::Doctor(args) => {
            doctor::run(&cli, args, writer).await?;
        }
        Command::Replay(args) => {
            let summary = replay::run(&cli, args).await?;
            writer
                .emit_success("replay", &summary)
                .map_err(CliError::Io)?;
        }
        Command::Backfill(args) => {
            let summary = backfill::run(&cli, args).await?;
            writer
                .emit_success("backfill", &summary)
                .map_err(CliError::Io)?;
        }
        Command::RebuildReport(args) => {
            let summary = rebuild_report::run(&cli, args).await?;
            writer
                .emit_success("rebuild-report", &summary)
                .map_err(CliError::Io)?;
        }
        Command::Reindex(args) => {
            let summary = reindex::run(&cli, args).await?;
            writer
                .emit_success("reindex", &summary)
                .map_err(CliError::Io)?;
        }
        Command::Migrate(args) => match args.action {
            MigrateAction::Run => {
                let summary = migrate::run(&cli).await?;
                writer
                    .emit_success("migrate", &summary)
                    .map_err(CliError::Io)?;
            }
            MigrateAction::Check => {
                let summary = migrate::check(&cli).await?;
                writer
                    .emit_success("migrate", &summary)
                    .map_err(CliError::Io)?;
            }
        },
        Command::Run(args) => {
            let summary = run::run(&cli, args).await?;
            writer.emit_success("run", &summary).map_err(CliError::Io)?;
        }
    }
    Ok(())
}
