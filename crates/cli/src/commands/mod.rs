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
            ai_run::run(args).await?;
        }
        Command::Publish(args) => {
            publish::run(args).await?;
        }
        Command::Doctor(args) => {
            doctor::run(args).await?;
        }
        Command::Replay(args) => {
            replay::run(args).await?;
        }
        Command::Backfill(args) => {
            backfill::run(args).await?;
        }
        Command::RebuildReport(args) => {
            rebuild_report::run(args).await?;
        }
        Command::Reindex(args) => {
            reindex::run(args).await?;
        }
        Command::Migrate(args) => match args.action {
            MigrateAction::Run => {
                migrate::run().await?;
            }
            MigrateAction::Check => {
                migrate::check().await?;
            }
        },
        Command::Run(args) => {
            run::run(args).await?;
        }
    }
    Ok(())
}
