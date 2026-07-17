use clap::Parser;
use rss_ai_news_cli::{
    args::{Cli, Command},
    commands::{
        ai_run::AiRunCommandSummary,
        backfill::{BackfillCommandSummary, parse_date_start, sha256_hex},
        migrate::MigrateCommandSummary,
        publish::{PublishCommandSummary, PublishStageOutcome},
        publish_all::{PublishAllCategorySummary, PublishAllCommandSummary},
        rebuild_report::RebuildReportCommandSummary,
        recent_entries::{
            RecentEntriesCommandSummary, RecentEntrySummary, RecentSourceHealthSummary,
        },
        reindex::{ReindexCommandSummary, ReindexMode, ReindexTargetOutcome},
        replay::ReplayCommandSummary,
        run::RunCommandSummary,
    },
    error::CliError,
    output::{CommandSummary, failure_envelope, success_envelope},
};
use serde_json::json;

#[test]
fn ai_run_summary_pretty_renders() {
    assert_pretty_contains(&ai_summary(), "AI run completed");
}

#[test]
fn ai_run_summary_serializes_json_fields() {
    assert_eq!(json_value(&ai_summary(), "process_succeeded"), 2);
}

#[test]
fn publish_summary_pretty_renders() {
    assert_pretty_contains(&publish_summary(), "Publish completed");
}

#[test]
fn publish_summary_serializes_stages() {
    let value = serde_json::to_value(publish_summary()).unwrap();
    assert_eq!(value["stages"][0]["stage"], "init");
}

#[test]
fn rebuild_report_summary_pretty_renders() {
    assert_pretty_contains(&rebuild_summary(), "Rebuild report completed");
}

#[test]
fn rebuild_report_summary_serializes_bytes() {
    assert_eq!(json_value(&rebuild_summary(), "markdown_bytes"), 42);
}

#[test]
fn migrate_summary_pretty_renders() {
    assert_pretty_contains(&migrate_summary(), "Migrate check completed");
}

#[test]
fn migrate_summary_serializes_current_version() {
    assert_eq!(json_value(&migrate_summary(), "current_version"), 2);
}

#[test]
fn recent_entries_summary_pretty_renders() {
    assert_pretty_contains(&recent_entries_summary(), "Recent entries for daily-math");
}

#[test]
fn recent_entries_json_envelope_matches_contract() {
    let envelope = success_envelope("recent-entries", &recent_entries_summary());

    assert_eq!(envelope["command"], "recent-entries");
    assert_eq!(envelope["status"], "success");
    assert_eq!(envelope["errors"], json!([]));
    assert_eq!(envelope["summary"]["schema_version"], 1);
    assert_eq!(envelope["summary"]["category"], "daily-math");
    assert_eq!(
        envelope["summary"]["discovered_after"],
        "2026-07-14T23:30:00Z"
    );
    assert_eq!(envelope["summary"]["entries"][0]["state"], "pending_fetch");
}

#[test]
fn recent_entries_output_redacts_large_or_sensitive_fields() {
    let value = serde_json::to_value(recent_entries_summary()).expect("serialize summary");
    let source = value["source_health"][0]
        .as_object()
        .expect("source object");
    let entry = value["entries"][0].as_object().expect("entry object");

    assert!(!source.contains_key("last_error"));
    assert!(!entry.contains_key("summary_raw"));
    assert!(!entry.contains_key("last_error"));
}

#[tokio::test]
async fn recent_entries_requires_category() {
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "recent-entries",
        "--discovered-after",
        "2026-07-14T23:30:00Z",
    ])
    .expect("parse");
    let Command::RecentEntries(args) = &cli.command else {
        panic!("expected recent-entries")
    };

    let error = rss_ai_news_cli::commands::recent_entries::run(&cli, args)
        .await
        .expect_err("missing category should fail before config I/O");

    assert_eq!(error.command_name(), "recent-entries");
    assert_eq!(error.error_kind(), "recent_entries_category_required");
    assert_eq!(error.exit_code().as_i32(), 2);
    let envelope = failure_envelope(error.command_name(), &error);
    assert_eq!(envelope["command"], "recent-entries");
    assert_eq!(envelope["status"], "error");
}

#[test]
fn recent_entries_command_context_preserves_underlying_error_classification() {
    let error = CliError::Runtime(rss_ai_news_runtime::RuntimeError::Config(
        "fixture failure".to_string(),
    ))
    .in_command("recent-entries");

    assert_eq!(error.command_name(), "recent-entries");
    assert_eq!(error.error_kind(), "runtime");
}

#[test]
fn replay_summary_pretty_renders() {
    assert_pretty_contains(&replay_summary(), "Replay completed");
}

#[test]
fn replay_summary_serializes_parsed_payload() {
    let value = serde_json::to_value(replay_summary()).unwrap();
    assert_eq!(value["parsed"]["entry_count"], 1);
}

#[test]
fn backfill_summary_pretty_renders() {
    assert_pretty_contains(&backfill_summary(), "Backfill completed");
}

#[test]
fn backfill_summary_serializes_inserted_count() {
    assert_eq!(json_value(&backfill_summary(), "ai_tasks_inserted"), 3);
}

#[test]
fn reindex_summary_pretty_renders() {
    assert_pretty_contains(&reindex_summary(), "Reindex completed");
}

#[test]
fn reindex_summary_serializes_archived_count() {
    // F5-6: ReindexCommandSummary 改为 `per_target: Vec<...>` 以支持
    // `--target=all`（§4.8 line 297）。`archived` 字段下钻到第 0 项。
    let value = serde_json::to_value(reindex_summary()).expect("serialize");
    assert_eq!(value["per_target"][0]["archived"].as_i64(), Some(1));
}

#[test]
fn run_summary_pretty_renders() {
    assert_pretty_contains(&run_summary(), "Run completed");
}

#[test]
fn run_summary_serializes_nested_publish() {
    let value = serde_json::to_value(run_summary()).unwrap();
    assert_eq!(value["publish"]["categories"][0]["publish_record_id"], 7);
}

#[test]
fn parse_date_start_accepts_yyyy_mm_dd() {
    assert!(parse_date_start(Some("2026-05-01")).unwrap().is_some());
}

#[test]
fn parse_date_start_accepts_none() {
    assert!(parse_date_start(None).unwrap().is_none());
}

#[test]
fn parse_date_start_rejects_bad_month() {
    assert!(parse_date_start(Some("2026-99-01")).is_err());
}

#[test]
fn sha256_hex_is_stable() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn replay_not_found_error_kind_is_specific() {
    let error = CliError::ReplayArtifactNotFound {
        kind: "feed_payload".to_string(),
        key: "missing".to_string(),
    };
    assert_eq!(error.error_kind(), "replay_artifact_not_found");
}

#[test]
fn publish_record_not_found_error_kind_is_specific() {
    let error = CliError::PublishRecordNotFound {
        idempotency_key: "k".to_string(),
    };
    assert_eq!(error.error_kind(), "publish_record_not_found");
}

#[test]
fn publish_conflict_error_kind_is_specific() {
    let error = CliError::PublishConflict {
        state: "failed".to_string(),
    };
    assert_eq!(error.error_kind(), "publish_conflict");
}

#[test]
fn ingest_specific_not_implemented_errors_are_split() {
    assert_eq!(
        CliError::DryRunNotImplemented.error_kind(),
        "dry_run_not_implemented"
    );
    assert_eq!(
        CliError::IngestSourceFilterNotImplemented.error_kind(),
        "ingest_source_not_implemented"
    );
}

fn ai_summary() -> AiRunCommandSummary {
    AiRunCommandSummary {
        task_gen_scanned: 4,
        task_gen_inserted: 3,
        task_gen_conflict_skipped: 1,
        process_claimed: 3,
        process_succeeded: 2,
        process_filtered: 1,
        process_retryable_failed: 0,
        process_permanent_failed: 0,
        process_tasks_panicked: 0,
        duration_seconds: 1.25,
    }
}

fn publish_summary() -> PublishCommandSummary {
    PublishCommandSummary {
        category: "ai".to_string(),
        date: "2026-05-01".to_string(),
        render_version: 5,
        publish_record_id: 7,
        mode: "local".to_string(),
        items: 2,
        local_path: Some("output/ai.md".to_string()),
        commit_sha: None,
        remote_target: None,
        stages: vec![PublishStageOutcome {
            stage: "init".to_string(),
            status: "created".to_string(),
        }],
        forced: false,
    }
}

fn publish_all_summary() -> PublishAllCommandSummary {
    PublishAllCommandSummary {
        date: "2026-05-01".to_string(),
        render_version: 5,
        mode: "local".to_string(),
        categories: vec![PublishAllCategorySummary {
            category: "ai".to_string(),
            publish_record_id: 7,
            items: 2,
            local_path: Some("output/ai.md".to_string()),
            commit_sha: None,
            remote_target: None,
            stages: vec![PublishStageOutcome {
                stage: "init".to_string(),
                status: "created".to_string(),
            }],
        }],
        commit_sha: None,
        forced: false,
    }
}

fn rebuild_summary() -> RebuildReportCommandSummary {
    RebuildReportCommandSummary {
        publish_record_id: 7,
        category: "ai".to_string(),
        date: "2026-05-01".to_string(),
        output_path: None,
        markdown_bytes: 42,
        items: 2,
    }
}

fn migrate_summary() -> MigrateCommandSummary {
    MigrateCommandSummary {
        action: "check".to_string(),
        applied_versions: vec![1, 2],
        current_version: Some(2),
    }
}

fn recent_entries_summary() -> RecentEntriesCommandSummary {
    RecentEntriesCommandSummary {
        schema_version: 1,
        generated_at: "2026-07-17T23:30:00Z".to_string(),
        category: "daily-math".to_string(),
        discovered_after: "2026-07-14T23:30:00Z".to_string(),
        limit: 50,
        truncated: false,
        source_health_truncated: false,
        source_health: vec![RecentSourceHealthSummary {
            source_key: "person.terence-tao.whats-new".to_string(),
            priority: 10,
            last_fetched_at: None,
            last_success_at: None,
            consecutive_failures: 0,
            last_error_kind: None,
        }],
        entries: vec![RecentEntrySummary {
            id: 1,
            source_key: "person.terence-tao.whats-new".to_string(),
            source_priority: 10,
            title: "Example".to_string(),
            url: "https://example.com/post".to_string(),
            published_at: None,
            discovered_at: "2026-07-17T22:00:00Z".to_string(),
            state: "pending_fetch".to_string(),
        }],
    }
}

fn replay_summary() -> ReplayCommandSummary {
    ReplayCommandSummary {
        kind: "feed_payload".to_string(),
        artifact_id: 1,
        artifact_key: "k".to_string(),
        byte_size: 10,
        parsed: json!({ "entry_count": 1 }),
        diff: None,
    }
}

fn backfill_summary() -> BackfillCommandSummary {
    BackfillCommandSummary {
        target: "ai".to_string(),
        date_from: None,
        date_to: None,
        feed_entries_examined: 0,
        feed_entries_reset: 0,
        new_prompt_version_id: Some(9),
        new_prompt_version_tag: Some("backfill-1700000000".to_string()),
        model_id: Some("gpt-4o-mini".to_string()),
        articles_scanned: 3,
        ai_tasks_inserted: 3,
        ai_tasks_conflict: 0,
    }
}

fn reindex_summary() -> ReindexCommandSummary {
    ReindexCommandSummary {
        mode: ReindexMode::Run,
        abort: None,
        per_target: vec![ReindexTargetOutcome {
            target: "categories".to_string(),
            new_rule_version_id: 11,
            reindex_job_id: 7,
            scanned: 2,
            updated: 2,
            unchanged: 0,
            conflict_skipped: 0,
            archived: 1,
            errors: 0,
        }],
    }
}

fn run_summary() -> RunCommandSummary {
    RunCommandSummary {
        ingest: Some(rss_ai_news_cli::commands::ingest::IngestCommandSummary {
            sources_attempted: 1,
            sources_succeeded: 1,
            sources_not_modified: 0,
            sources_failed: 0,
            entries_discovered: 1,
            entries_inserted: 1,
            articles_persisted: 1,
            articles_fallback: 0,
            fetch_failed: 0,
            tasks_panicked: 0,
            duration_seconds: 1.0,
        }),
        ai_run: Some(ai_summary()),
        publish: Some(publish_all_summary()),
        stage_failures: Vec::new(),
        ai_run_skip_reason: None,
        overall_duration_seconds: 2.0,
    }
}

fn assert_pretty_contains(summary: &impl CommandSummary, needle: &str) {
    let mut buf = Vec::new();
    summary.render_pretty(&mut buf).unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(text.contains(needle));
}

fn json_value(summary: &impl serde::Serialize, key: &str) -> i64 {
    serde_json::to_value(summary).unwrap()[key]
        .as_i64()
        .unwrap()
}
