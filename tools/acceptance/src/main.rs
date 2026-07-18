use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand, ValueEnum};
use rss_ai_news_acceptance::{Lane, MatrixReport, Profile, RunOptions, Status, runner::run_matrix};

#[derive(Debug, Parser)]
#[command(
    name = "rss-ai-news-acceptance",
    version,
    about = "Rust-native RSS-AI-News acceptance matrix orchestrator"
)]
struct Cli {
    #[arg(long, default_value = ".")]
    repo_root: PathBuf,

    #[arg(long)]
    target_dir: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Pretty)]
    format: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Pretty,
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the registered profiles and lanes without running them.
    List,
    /// Run a profile or one or more explicit lanes.
    Run {
        #[arg(long, value_enum, conflicts_with = "lane")]
        profile: Option<Profile>,

        #[arg(long, value_enum)]
        lane: Vec<Lane>,

        #[arg(long)]
        expected_version: Option<String>,

        #[arg(long)]
        dry_run: bool,

        #[arg(long)]
        fail_fast: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::List => {
            print_catalog(cli.format);
            ExitCode::SUCCESS
        }
        Command::Run {
            profile,
            lane,
            expected_version,
            dry_run,
            fail_fast,
        } => match run_matrix(RunOptions {
            repo_root: cli.repo_root,
            target_dir: cli.target_dir,
            lanes: lane,
            profile,
            expected_version,
            dry_run,
            fail_fast,
        }) {
            Ok(report) => {
                print_report(cli.format, &report);
                if report.succeeded() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(error) => {
                match cli.format {
                    OutputFormat::Pretty => eprintln!("acceptance matrix error: {error}"),
                    OutputFormat::Json => println!(
                        "{}",
                        serde_json::json!({
                            "schema_version": 1,
                            "status": "failed",
                            "error": error,
                        })
                    ),
                }
                ExitCode::FAILURE
            }
        },
    }
}

fn print_catalog(format: OutputFormat) {
    match format {
        OutputFormat::Pretty => {
            println!("Profiles:");
            for profile in [Profile::Local, Profile::Full] {
                let lanes = profile
                    .lanes()
                    .iter()
                    .map(|lane| lane.id())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  {:<5} {lanes}", profile.id());
            }
            println!("\nLanes:");
            for lane in Lane::ALL {
                println!("  {:<10} {}", lane.id(), lane.description());
            }
        }
        OutputFormat::Json => {
            let profiles = [Profile::Local, Profile::Full]
                .into_iter()
                .map(|profile| {
                    serde_json::json!({
                        "id": profile.id(),
                        "lanes": profile.lanes(),
                    })
                })
                .collect::<Vec<_>>();
            let lanes = Lane::ALL
                .into_iter()
                .map(|lane| {
                    serde_json::json!({
                        "id": lane.id(),
                        "description": lane.description(),
                    })
                })
                .collect::<Vec<_>>();
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "profiles": profiles,
                    "lanes": lanes,
                })
            );
        }
    }
}

fn print_report(format: OutputFormat, report: &MatrixReport) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(report).expect("matrix report serialization")
        ),
        OutputFormat::Pretty => {
            println!(
                "Acceptance matrix: {:?} (version {}, {} ms)",
                report.status, report.expected_version, report.duration_ms
            );
            println!("Repo: {}", report.repo_root);
            println!("Target: {}", report.target_dir);
            for lane in &report.lanes {
                println!(
                    "\n[{:?}] {} ({} ms)",
                    lane.status,
                    lane.lane.id(),
                    lane.duration_ms
                );
                for step in &lane.steps {
                    println!("  [{:?}] {} — {}", step.status, step.id, step.command);
                    if let Some(error) = &step.error {
                        println!("    error: {error}");
                    }
                    if let Some(stdout) = &step.stdout_tail {
                        println!("    stdout tail:\n{}", indent(stdout));
                    }
                    if let Some(stderr) = &step.stderr_tail {
                        println!("    stderr tail:\n{}", indent(stderr));
                    }
                }
            }
            if report.status == Status::Failed {
                eprintln!("\nAcceptance matrix failed.");
            }
        }
    }
}

fn indent(value: &str) -> String {
    value
        .lines()
        .map(|line| format!("      {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
