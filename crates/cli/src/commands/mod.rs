use crate::{
    args::{Cli, Command, MigrateAction},
    error::CliError,
    exit_code::ExitCode,
    output::OutputWriter,
};

pub mod ai_run;
pub mod backfill;
pub mod doctor;
pub mod ingest;
pub mod migrate;
pub mod publish;
pub mod publish_all;
pub mod rebuild_report;
pub mod recent_entries;
pub mod reindex;
pub mod replay;
pub mod run;
pub mod validate_config;

/// Invocation-level checks that must pass before anything with a side effect
/// happens (log file, metrics listener, storage): a global --dry-run that a
/// command silently ignored would perform the effects the user asked to skip.
pub fn check_invocation(cli: &Cli) -> Result<(), CliError> {
    if cli.dry_run && !cli.command.accepts_dry_run() {
        return Err(CliError::DryRunUnsupported {
            command: cli.command.name(),
        });
    }
    Ok(())
}

/// Runs the selected command and emits its summary exactly once.
///
/// A command either fails before producing a summary (`Err`, rendered once by
/// `lib::run` as a failure envelope) or returns a summary whose
/// [`CommandSummary::exit_code`](crate::output::CommandSummary::exit_code)
/// carries stage-level failures (e.g. `publish` store-local failed, `doctor`
/// found a failing check, `run` aggregated a stage failure). Never both.
pub async fn dispatch(cli: Cli, writer: &mut OutputWriter) -> Result<ExitCode, CliError> {
    check_invocation(&cli)?;
    match &cli.command {
        Command::ValidateConfig => {
            writer.emit_summary("validate-config", &validate_config::run(&cli).await?)
        }
        Command::Ingest(args) => writer.emit_summary("ingest", &ingest::run(&cli, args).await?),
        Command::AiRun(args) => writer.emit_summary("ai-run", &ai_run::run(&cli, args).await?),
        Command::Publish(args) => writer.emit_summary("publish", &publish::run(&cli, args).await?),
        Command::PublishAll(args) => {
            writer.emit_summary("publish-all", &publish_all::run(&cli, args).await?)
        }
        Command::Doctor(args) => writer.emit_summary("doctor", &doctor::run(&cli, args).await?),
        Command::Replay(args) => writer.emit_summary("replay", &replay::run(&cli, args).await?),
        Command::Backfill(args) => {
            writer.emit_summary("backfill", &backfill::run(&cli, args).await?)
        }
        Command::RebuildReport(args) => {
            writer.emit_summary("rebuild-report", &rebuild_report::run(&cli, args).await?)
        }
        Command::Reindex(args) => writer.emit_summary("reindex", &reindex::run(&cli, args).await?),
        Command::RecentEntries(args) => writer.emit_summary(
            recent_entries::COMMAND_NAME,
            &recent_entries::run(&cli, args).await?,
        ),
        Command::Migrate(args) => match args.action {
            MigrateAction::Run => writer.emit_summary("migrate", &migrate::run(&cli).await?),
            MigrateAction::Check => writer.emit_summary("migrate", &migrate::check(&cli).await?),
        },
        Command::Run(args) => writer.emit_summary("run", &run::run(&cli, args).await?),
    }
}
