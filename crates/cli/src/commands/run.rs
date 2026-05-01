use std::{
    io::{self, Write},
    time::Instant,
};

use serde::Serialize;

use crate::{
    args::{AiRunArgs, Cli, IngestArgs, PublishArgs, RunArgs},
    commands::{ai_run, ingest, publish},
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct RunCommandSummary {
    pub ingest: ingest::IngestCommandSummary,
    pub ai_run: ai_run::AiRunCommandSummary,
    pub publish: Option<publish::PublishCommandSummary>,
    pub overall_duration_seconds: f64,
}

impl CommandSummary for RunCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Run completed:")?;
        writeln!(
            writer,
            "  Ingest entries inserted: {}",
            self.ingest.entries_inserted
        )?;
        writeln!(
            writer,
            "  AI claimed:              {}",
            self.ai_run.process_claimed
        )?;
        if let Some(publish) = &self.publish {
            writeln!(
                writer,
                "  Publish record:          {}",
                publish.publish_record_id
            )?;
        } else {
            writeln!(writer, "  Publish:                 skipped or failed")?;
        }
        writeln!(
            writer,
            "  Duration:                {:.2}s",
            self.overall_duration_seconds
        )
    }
}

pub async fn run(cli: &Cli, args: &RunArgs) -> Result<RunCommandSummary, CliError> {
    let started = Instant::now();
    let ingest_args = IngestArgs {
        batch_size: args.ingest_batch_size.unwrap_or(50),
        ..IngestArgs::default()
    };
    let ai_args = AiRunArgs {
        batch_size: args.ai_batch_size.unwrap_or(20),
        model: None,
    };
    let publish_args = PublishArgs {
        date: args.publish_date.clone(),
        local_only: false,
        force: false,
    };

    let ingest = ingest::run(cli, &ingest_args).await?;
    let ai_run = ai_run::run(cli, &ai_args).await?;
    let publish = publish::run(cli, &publish_args).await.ok();

    Ok(RunCommandSummary {
        ingest,
        ai_run,
        publish,
        overall_duration_seconds: started.elapsed().as_secs_f64(),
    })
}
