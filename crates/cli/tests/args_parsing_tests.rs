use clap::{CommandFactory, Parser, error::ErrorKind};
use rss_ai_news_cli::args::{
    BackfillTarget, Cli, Command, LogFormat, MigrateAction, OutputFormat, ReindexTarget, ReplayKind,
};

#[tokio::test]
async fn args_parsing_parses_validate_config() {
    let cli = Cli::try_parse_from(["rss-ai-news", "validate-config"]).expect("parse");
    assert!(matches!(cli.command, Command::ValidateConfig));
}

#[tokio::test]
async fn args_parsing_parses_ingest_with_defaults() {
    let cli = Cli::try_parse_from(["rss-ai-news", "ingest"]).expect("parse");
    match cli.command {
        Command::Ingest(args) => {
            assert_eq!(args.batch_size, 50);
            assert!(!args.skip_fetch);
            assert_eq!(args.source, None);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_ingest_with_custom_flags() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "ingest",
        "--batch-size",
        "20",
        "--skip-fetch",
    ])
    .expect("parse");
    match cli.command {
        Command::Ingest(args) => {
            assert_eq!(args.batch_size, 20);
            assert!(args.skip_fetch);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_ai_run_args() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "ai-run",
        "--batch-size",
        "7",
        "--model",
        "test-model",
    ])
    .expect("parse");
    match cli.command {
        Command::AiRun(args) => {
            assert_eq!(args.batch_size, 7);
            assert_eq!(args.model.as_deref(), Some("test-model"));
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_publish_with_local_only_and_force() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "publish",
        "--date",
        "2026-04-30",
        "--local-only",
        "--force",
    ])
    .expect("parse");
    match cli.command {
        Command::Publish(args) => {
            assert_eq!(args.date.as_deref(), Some("2026-04-30"));
            assert!(args.local_only);
            assert!(args.force);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_doctor_with_deep() {
    let cli = Cli::try_parse_from(["rss-ai-news", "doctor", "--deep"]).expect("parse");
    match cli.command {
        Command::Doctor(args) => assert!(args.deep),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_replay_with_kind_and_key() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "replay",
        "--kind",
        "feed",
        "--key",
        "source-1",
        "--diff",
    ])
    .expect("parse");
    match cli.command {
        Command::Replay(args) => {
            assert_eq!(args.kind, ReplayKind::Feed);
            assert_eq!(args.key.as_deref(), Some("source-1"));
            assert!(args.diff);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_replay_kind_id_conflicts() {
    let error = Cli::try_parse_from([
        "rss-ai-news",
        "replay",
        "--kind",
        "ai",
        "--key",
        "k",
        "--id",
        "1",
    ])
    .expect_err("conflict fails");
    assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
}

#[tokio::test]
async fn args_parsing_parses_backfill_with_target_extract() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "backfill",
        "--target",
        "extract",
        "--date-from",
        "2026-04-01",
        "--date-to",
        "2026-04-30",
    ])
    .expect("parse");
    match cli.command {
        Command::Backfill(args) => {
            assert_eq!(args.target, BackfillTarget::Extract);
            assert_eq!(args.batch_size, 50);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_rebuild_report_with_publish_id() {
    let cli = Cli::try_parse_from(["rss-ai-news", "rebuild-report", "--publish-id", "42"])
        .expect("parse");
    match cli.command {
        Command::RebuildReport(args) => assert_eq!(args.publish_id, Some(42)),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_reindex_with_target_link_hash() {
    let cli =
        Cli::try_parse_from(["rss-ai-news", "reindex", "--target", "link_hash"]).expect("parse");
    match cli.command {
        Command::Reindex(args) => {
            assert_eq!(args.target, ReindexTarget::LinkHash);
            assert_eq!(args.batch_size, 100);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_migrate_run_subcommand() {
    let cli = Cli::try_parse_from(["rss-ai-news", "migrate", "run"]).expect("parse");
    match cli.command {
        Command::Migrate(args) => assert!(matches!(args.action, MigrateAction::Run)),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_migrate_check_subcommand() {
    let cli = Cli::try_parse_from(["rss-ai-news", "migrate", "check"]).expect("parse");
    match cli.command {
        Command::Migrate(args) => assert!(matches!(args.action, MigrateAction::Check)),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_top_level_run_subcommand() {
    let cli = Cli::try_parse_from(["rss-ai-news", "run"]).expect("parse");
    assert!(matches!(cli.command, Command::Run(_)));
}

#[tokio::test]
async fn args_parsing_parses_global_flags_before_subcommand() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "--config-dir",
        "foo",
        "--log-format",
        "json",
        "--category",
        "ai",
        "ingest",
        "--batch-size",
        "10",
    ])
    .expect("parse");
    assert_eq!(cli.config_dir.to_string_lossy(), "foo");
    assert_eq!(cli.log_format, LogFormat::Json);
    assert_eq!(cli.category.as_deref(), Some("ai"));
    match cli.command {
        Command::Ingest(args) => assert_eq!(args.batch_size, 10),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_parses_output_format_json() {
    let cli = Cli::try_parse_from(["rss-ai-news", "-o", "json", "validate-config"]).expect("parse");
    assert_eq!(cli.output_format, OutputFormat::Json);
}

#[tokio::test]
async fn args_parsing_unknown_subcommand_fails_with_exit_code_2() {
    let error = Cli::try_parse_from(["rss-ai-news", "garbage-command"]).expect_err("fails");
    assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
    assert_eq!(error.exit_code(), 2);
}

#[tokio::test]
async fn args_parsing_help_lists_all_top_level_subcommands() {
    let mut command = Cli::command();
    let help = command.render_long_help().to_string();
    for name in [
        "ingest",
        "ai-run",
        "publish",
        "doctor",
        "replay",
        "backfill",
        "rebuild-report",
        "reindex",
        "migrate",
        "validate-config",
        "run",
    ] {
        assert!(help.contains(name), "help should contain {name}");
    }
}
