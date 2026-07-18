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
async fn args_parsing_parses_publish_all_with_shared_publish_args() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "publish-all",
        "--date",
        "2026-04-30",
        "--force",
    ])
    .expect("parse");
    match cli.command {
        Command::PublishAll(args) => {
            assert_eq!(args.date.as_deref(), Some("2026-04-30"));
            assert!(!args.local_only);
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
            assert_eq!(args.target, Some(ReindexTarget::LinkHash));
            assert_eq!(args.batch_size, 100);
            assert!(args.abort.is_none());
            assert!(!args.dry_run);
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

#[test]
fn args_parsing_parses_recent_entries_defaults() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "--category",
        "daily-math",
        "recent-entries",
        "--discovered-after",
        "1970-01-01T00:00:00Z",
    ])
    .expect("parse recent-entries");
    match cli.command {
        Command::RecentEntries(args) => {
            assert_eq!(args.discovered_after.unix_timestamp(), 0);
            assert!(args.published_after.is_none());
            assert_eq!(
                args.limit,
                rss_ai_news_runtime::DEFAULT_RECENT_ENTRIES_LIMIT
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn args_parsing_parses_recent_entries_published_after() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "recent-entries",
        "--discovered-after",
        "1970-01-01T00:00:00Z",
        "--published-after",
        "1970-01-02T00:00:00+00:00",
    ])
    .expect("parse recent-entries published cutoff");
    match cli.command {
        Command::RecentEntries(args) => {
            assert_eq!(args.published_after.unwrap().unix_timestamp(), 86_400);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn args_parsing_rejects_recent_entries_invalid_timestamp() {
    let error = Cli::try_parse_from([
        "rss-ai-news",
        "recent-entries",
        "--discovered-after",
        "not-a-timestamp",
    ])
    .expect_err("invalid RFC3339 should fail");
    assert_eq!(error.kind(), ErrorKind::ValueValidation);
    assert_eq!(error.exit_code(), 2);
}

#[test]
fn args_parsing_rejects_recent_entries_limit_out_of_range() {
    for limit in ["0", "201"] {
        let error = Cli::try_parse_from([
            "rss-ai-news",
            "recent-entries",
            "--discovered-after",
            "1970-01-01T00:00:00Z",
            "--limit",
            limit,
        ])
        .expect_err("out-of-range limit should fail");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert_eq!(error.exit_code(), 2);
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
        "publish-all",
        "doctor",
        "replay",
        "backfill",
        "rebuild-report",
        "reindex",
        "recent-entries",
        "migrate",
        "validate-config",
        "run",
    ] {
        assert!(help.contains(name), "help should contain {name}");
    }
}

// === F5-6 W2-A-3 / W2-B-5: 补 CLI 标志（max-batches / reindex target=all / abort）===
// F7-1 W3-2 修复：--max-batches 不再以 global=true 挂在 Cli 上，而是各
// 子命令本地定义。下列测试在子命令侧 args 上验证。

#[tokio::test]
async fn args_parsing_max_batches_accepted_on_ingest_subcommand() {
    // cli-semantics §4.1 line 62: ingest 接受 --max-batches。
    let cli = Cli::try_parse_from(["rss-ai-news", "ingest", "--max-batches", "3"]).expect("parse");
    match cli.command {
        Command::Ingest(args) => assert_eq!(args.max_batches, Some(3)),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_max_batches_accepted_on_ai_run_subcommand() {
    // cli-semantics §4.2 line 97: ai-run 接受 --max-batches。
    let cli = Cli::try_parse_from(["rss-ai-news", "ai-run", "--max-batches", "7"]).expect("parse");
    match cli.command {
        Command::AiRun(args) => assert_eq!(args.max_batches, Some(7)),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_max_batches_accepted_on_run_subcommand() {
    // cli-semantics §4.11 line 358: run 自身接受 --max-batches，
    // 内部 ingest/ai-run 阶段沿用同一生效值（无 --ingest-max-batches）。
    let cli = Cli::try_parse_from(["rss-ai-news", "run", "--max-batches", "5"]).expect("parse");
    match cli.command {
        Command::Run(args) => assert_eq!(args.max_batches, Some(5)),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_max_batches_zero_means_unlimited() {
    // config-schema §4.4 line 196: 0 = 不限。clap 应正常接受 0，不
    // 折叠为缺省。
    let cli = Cli::try_parse_from(["rss-ai-news", "ingest", "--max-batches", "0"]).expect("parse");
    match cli.command {
        Command::Ingest(args) => assert_eq!(args.max_batches, Some(0)),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_max_batches_rejected_on_unsupporting_subcommand() {
    // F7-1 W3-2: --max-batches 不再是全局 flag，publish / doctor / reindex
    // 等子命令的 --help 不再显示该标志，传入应被 clap 拒绝。
    // 这是 surface 收敛的关键守护：避免静默忽略。
    let err = Cli::try_parse_from(["rss-ai-news", "publish", "--max-batches", "3"])
        .expect_err("publish does not accept --max-batches");
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
    assert_eq!(err.exit_code(), 2);

    let err = Cli::try_parse_from(["rss-ai-news", "doctor", "--max-batches", "3"])
        .expect_err("doctor does not accept --max-batches");
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);

    let err = Cli::try_parse_from(["rss-ai-news", "validate-config", "--max-batches", "3"])
        .expect_err("validate-config does not accept --max-batches");
    assert_eq!(err.kind(), ErrorKind::UnknownArgument);
}

#[tokio::test]
async fn args_parsing_reindex_target_all_parses() {
    // cli-semantics §4.8 line 287: --target=all 是合法值（顺序执行三类）。
    let cli = Cli::try_parse_from(["rss-ai-news", "reindex", "--target", "all"]).expect("parse");
    match cli.command {
        Command::Reindex(args) => {
            assert_eq!(args.target, Some(ReindexTarget::All));
            assert!(args.abort.is_none());
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_reindex_target_all_expands_to_three_domain_targets_in_order() {
    // §4.8 line 297: target='all' 时按顺序生成三个独立 job
    // (link_hash → content_hash → categories)。
    use rss_ai_news_domain::state::ReindexTarget as DomainTarget;
    let expanded = ReindexTarget::All.expand();
    assert_eq!(
        expanded,
        vec![
            DomainTarget::LinkHash,
            DomainTarget::ContentHash,
            DomainTarget::Categories,
        ],
    );
}

#[tokio::test]
async fn args_parsing_reindex_abort_parses_without_target() {
    // §4.8 line 290: --abort <job_id>，与 --target 互斥（用户应可省略 --target）。
    let cli = Cli::try_parse_from(["rss-ai-news", "reindex", "--abort", "job-42"]).expect("parse");
    match cli.command {
        Command::Reindex(args) => {
            assert_eq!(args.abort.as_deref(), Some("job-42"));
            assert!(args.target.is_none());
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_reindex_abort_and_target_are_mutually_exclusive() {
    // §4.8 line 290 + clap conflicts_with：同时提供应 parse 失败 (exit 2)。
    let err = Cli::try_parse_from([
        "rss-ai-news",
        "reindex",
        "--target",
        "link_hash",
        "--abort",
        "x",
    ])
    .expect_err("conflict");
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    assert_eq!(err.exit_code(), 2);
}

#[tokio::test]
async fn args_parsing_reindex_without_target_or_abort_is_rejected() {
    // §4.8 line 287: --target 必填（除非 --abort）。
    // clap required_unless_present 应直接 reject。
    let err = Cli::try_parse_from(["rss-ai-news", "reindex"]).expect_err("missing target");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    assert_eq!(err.exit_code(), 2);
}

// === F5-7 W2-A-6: BackfillArgs 版本 override 字段 ===

#[tokio::test]
async fn args_parsing_backfill_accepts_version_override_fields() {
    // cli-semantics §4.6 + state-machine §4.4: backfill ai 创建新版本任务行。
    // 用户应能命名版本（reproducibility）、切换 model（A/B 实验）。
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "backfill",
        "--target",
        "ai",
        "--prompt-version-tag",
        "exp-2026-05",
        "--prompt-version-description",
        "rerun after prompt v2 tweak",
        "--model",
        "gpt-4o",
    ])
    .expect("parse");
    match cli.command {
        Command::Backfill(args) => {
            assert!(matches!(args.target, BackfillTarget::Ai));
            assert_eq!(args.prompt_version_tag.as_deref(), Some("exp-2026-05"));
            assert_eq!(
                args.prompt_version_description.as_deref(),
                Some("rerun after prompt v2 tweak"),
            );
            assert_eq!(args.model.as_deref(), Some("gpt-4o"));
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_backfill_version_override_fields_default_to_none() {
    // 缺省时全部为 None；命令体内回落到生成 tag / config model。
    let cli = Cli::try_parse_from(["rss-ai-news", "backfill", "--target", "ai"]).expect("parse");
    match cli.command {
        Command::Backfill(args) => {
            assert!(args.prompt_version_tag.is_none());
            assert!(args.prompt_version_description.is_none());
            assert!(args.model.is_none());
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[tokio::test]
async fn args_parsing_max_batches_flows_into_cli_overrides_from_ingest() {
    // config-schema §8 line 405: --max-batches 应通过 CliOverrides 落到
    // app.runtime.max_batches_per_run。F7-1 W3-2 之后，该标志只在
    // ingest / ai-run / run 子命令暴露；Cli::to_cli_overrides 通过 match
    // command variant 从对应子命令 args 提取。
    let cli = Cli::try_parse_from(["rss-ai-news", "ingest", "--max-batches", "4"]).expect("parse");
    let overrides = cli.to_cli_overrides();
    assert_eq!(overrides.max_batches, Some(4));
}

#[tokio::test]
async fn args_parsing_max_batches_flows_into_cli_overrides_from_ai_run() {
    let cli = Cli::try_parse_from(["rss-ai-news", "ai-run", "--max-batches", "6"]).expect("parse");
    let overrides = cli.to_cli_overrides();
    assert_eq!(overrides.max_batches, Some(6));
}

#[tokio::test]
async fn args_parsing_max_batches_flows_into_cli_overrides_from_run() {
    let cli = Cli::try_parse_from(["rss-ai-news", "run", "--max-batches", "2"]).expect("parse");
    let overrides = cli.to_cli_overrides();
    assert_eq!(overrides.max_batches, Some(2));
}

#[tokio::test]
async fn args_parsing_max_batches_in_overrides_is_none_for_unsupporting_subcommand() {
    // F7-1 W3-2: 非 ingest/ai-run/run 子命令的 CliOverrides.max_batches
    // 固定为 None；这保证 config 层不会被错误的 override 注入。
    let cli = Cli::try_parse_from(["rss-ai-news", "validate-config"]).expect("parse");
    let overrides = cli.to_cli_overrides();
    assert_eq!(overrides.max_batches, None);

    let cli = Cli::try_parse_from(["rss-ai-news", "publish"]).expect("parse");
    let overrides = cli.to_cli_overrides();
    assert_eq!(overrides.max_batches, None);
}
