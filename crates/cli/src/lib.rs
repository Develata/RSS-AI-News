//! Command interface: clap-based parser + output formatting.
//! See docs/design/cli-semantics.md for the 11 commands.

// TODO Phase 1:
// - mod error;
// - mod args;           // clap derive: Cli, Command enum
// - mod commands {
//       ingest, ai_run, publish, doctor, replay, backfill,
//       rebuild_report, reindex, migrate, validate_config, run,
//   }
// - mod output;         // pretty vs json formatter
// - mod exit_code;      // stable exit-code mapping

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // placeholder until Phase 1
    Ok(())
}
