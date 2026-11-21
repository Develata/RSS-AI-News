//! `run` command — orchestrates ingest → ai-run → publish in one invocation.
//!
//! Per `docs-backup/design/cli-semantics.md` §4.11 and docs/plan/11 §5: any
//! single stage failing must NOT block downstream stages. An ingest failure
//! (its preflight, e.g. a missing `RSSHUB_BASE_URL`, or the ingest itself
//! returning `Err`) is recorded and ai-run and publish still run on what the
//! database already holds, so one misconfigured source cannot hold back every
//! category's report. Only a stage's own preconditions skip it: when the
//! database cannot be opened (or migrated) no stage can run. The overall exit
//! code reflects the most severe stage outcome; failures are surfaced to the
//! user via the run summary, not silently dropped.
//!
//! §4.11 lines 362-368 carve-out: when `app.ai.enabled = false`, `run`
//! must **proactively skip** `ai-run` (emit one INFO log line, do NOT
//! return exit 78) and proceed straight to `publish` per the
//! `(ai=false, include_unscored)` truth table. This differs from the
//! standalone `ai-run` invocation which intentionally fails with exit 78
//! because the user explicitly requested an action that contradicts the
//! current configuration.

use std::{
    io::{self, Write},
    time::Instant,
};

use rss_ai_news_config::{self as config, LoadedConfig};
use serde::Serialize;

use crate::{
    args::{AiRunArgs, Cli, IngestArgs, PublishArgs, RunArgs},
    commands::{ai_run, ingest, publish_all},
    context_factory::open_write_storage,
    error::CliError,
    exit_code::ExitCode,
    output::{CommandSummary, RenderedError},
};

/// Reason an ai-run stage was skipped (rather than executed-and-failed):
/// the §4.11 `ai.enabled=false` branch.
pub const AI_RUN_SKIP_REASON_DISABLED: &str = "ai.enabled=false";
/// ai-run (like every stage) was skipped because the database could not be
/// opened; that failure is recorded once, under the `ingest` stage.
pub const AI_RUN_SKIP_REASON_NO_DATABASE: &str = "database unavailable";

#[derive(Debug, Clone, Serialize)]
pub struct StageFailure {
    pub stage: &'static str,
    pub error_kind: String,
    pub message: String,
    pub exit_code_value: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunCommandSummary {
    pub ingest: Option<ingest::IngestCommandSummary>,
    pub ai_run: Option<ai_run::AiRunCommandSummary>,
    pub publish: Option<publish_all::PublishAllCommandSummary>,
    pub stage_failures: Vec<StageFailure>,
    /// `Some(reason)` iff the ai-run stage was skipped rather than run:
    /// deliberately per §4.11 (`ai.enabled=false`), or because the database
    /// was unavailable. Distinguishes the "skipped" branch from the
    /// "executed and failed" branch — both surface as `ai_run = None`, but
    /// only the latter pushes an `ai-run` `StageFailure`. `None` means the
    /// stage ran (regardless of outcome).
    pub ai_run_skip_reason: Option<&'static str>,
    pub overall_duration_seconds: f64,
}

impl RunCommandSummary {
    /// Most severe exit code across all stage failures. `Success` if no
    /// failures were recorded. Severity ordering follows the numeric
    /// `ExitCode::as_i32()` value (higher = more severe), so a config
    /// failure (78) outranks a runtime failure (1).
    pub fn derive_exit_code(&self) -> ExitCode {
        self.stage_failures
            .iter()
            .map(|failure| failure.exit_code_value)
            .max()
            .map_or(ExitCode::Success, ExitCode::from_i32)
    }
}

impl CommandSummary for RunCommandSummary {
    fn exit_code(&self) -> ExitCode {
        self.derive_exit_code()
    }

    fn errors(&self) -> Vec<RenderedError> {
        self.stage_failures
            .iter()
            .map(|failure| RenderedError {
                kind: format!("{}_{}", failure.stage, failure.error_kind),
                message: format!("[{}] {}", failure.stage, failure.message),
            })
            .collect()
    }

    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        if self.stage_failures.is_empty() {
            writeln!(writer, "Run completed:")?;
        } else {
            writeln!(writer, "Run failed:")?;
        }
        match &self.ingest {
            Some(ingest) => writeln!(
                writer,
                "  Ingest entries inserted: {}",
                ingest.entries_inserted
            )?,
            None => writeln!(writer, "  Ingest:                  failed")?,
        }
        match (&self.ai_run, self.ai_run_skip_reason) {
            (Some(ai_run), _) => writeln!(
                writer,
                "  AI claimed:              {}",
                ai_run.process_claimed
            )?,
            (None, Some(reason)) => {
                writeln!(writer, "  AI:                      skipped ({reason})")?
            }
            (None, None) => writeln!(writer, "  AI:                      failed")?,
        }
        match &self.publish {
            Some(publish) => writeln!(
                writer,
                "  Publish records:         {}",
                publish.categories.len()
            )?,
            None => writeln!(writer, "  Publish:                 skipped or failed")?,
        }
        writeln!(
            writer,
            "  Duration:                {:.2}s",
            self.overall_duration_seconds
        )?;
        if !self.stage_failures.is_empty() {
            writeln!(writer, "Stage failures ({}):", self.stage_failures.len())?;
            for failure in &self.stage_failures {
                writeln!(
                    writer,
                    "  [{}] ({}) {}",
                    failure.stage, failure.error_kind, failure.message
                )?;
            }
        }
        Ok(())
    }
}

fn record_stage_failure(failures: &mut Vec<StageFailure>, stage: &'static str, error: &CliError) {
    failures.push(StageFailure {
        stage,
        error_kind: error.error_kind().to_string(),
        message: error.display_user(),
        exit_code_value: error.exit_code().as_i32(),
    });
}

/// Records the failures a stage reported through its summary (it returned
/// `Ok` but its own exit code is non-zero, e.g. publish store-local failed).
fn record_summary_failures<S: CommandSummary>(
    failures: &mut Vec<StageFailure>,
    stage: &'static str,
    summary: &S,
) {
    let exit_code = summary.exit_code();
    if exit_code == ExitCode::Success {
        return;
    }
    let errors = summary.errors();
    if errors.is_empty() {
        failures.push(StageFailure {
            stage,
            error_kind: "stage_failed".to_string(),
            message: format!("{stage} reported status {}", summary.status()),
            exit_code_value: exit_code.as_i32(),
        });
    }
    for error in errors {
        failures.push(StageFailure {
            stage,
            error_kind: error.kind,
            message: error.message,
            exit_code_value: exit_code.as_i32(),
        });
    }
}

pub async fn run(cli: &Cli, args: &RunArgs) -> Result<RunCommandSummary, CliError> {
    let started = Instant::now();

    // The config is loaded and the storage pool opened exactly once, so every
    // stage of one run sees the same configuration (no hot reload within a
    // process, docs/plan/13) and migrations / config-version checks run once.
    //
    // Only structural checks run here; environment checks belong to each
    // stage (ingest / publish preflight, per-category AI credentials), so one
    // category missing credentials is an ai-run stage failure (exit 78 in the
    // summary) instead of aborting ingest and publish for every category.
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())
        .map_err(CliError::Config)?;
    run_loaded(&loaded, args, started).await
}

/// The stages of `run` on an already loaded config (split from [`run`] so
/// tests can control the environment snapshot in `LoadedConfig::env`).
async fn run_loaded(
    loaded: &LoadedConfig,
    args: &RunArgs,
    started: Instant,
) -> Result<RunCommandSummary, CliError> {
    let ai_enabled = loaded.app.ai.enabled;

    // `run` 内部 ingest / ai-run 共用同一 --max-batches 生效值（已由
    // CliOverrides 写入 loaded.app.runtime.max_batches_per_run）。
    let ingest_args = IngestArgs {
        batch_size: args.ingest_batch_size.unwrap_or(50),
        max_batches: args.max_batches,
        ..IngestArgs::default()
    };
    let ai_args = AiRunArgs {
        batch_size: args.ai_batch_size.unwrap_or(20),
        model: None,
        max_batches: args.max_batches,
    };
    let publish_args = PublishArgs {
        date: args.publish_date.clone(),
        local_only: false,
        force: false,
    };

    let mut stage_failures: Vec<StageFailure> = Vec::new();
    let mut ai_run_skip_reason: Option<&'static str> = None;

    // Ingest's own precondition: a failure is recorded and only ingest is
    // skipped; ai-run and publish still work on the stored state.
    let ingest_ready = match ingest::preflight(loaded) {
        Ok(()) => true,
        Err(err) => {
            record_stage_failure(&mut stage_failures, "ingest", &err);
            false
        }
    };
    // Every stage's precondition: without a usable database nothing can run.
    // Reported under `ingest` (the first stage), as before.
    let pool = match open_write_storage(loaded).await {
        Ok(pool) => pool,
        Err(err) => {
            record_stage_failure(&mut stage_failures, "ingest", &err);
            return Ok(RunCommandSummary {
                ingest: None,
                ai_run: None,
                publish: None,
                stage_failures,
                ai_run_skip_reason: Some(AI_RUN_SKIP_REASON_NO_DATABASE),
                overall_duration_seconds: started.elapsed().as_secs_f64(),
            });
        }
    };
    let pool = &pool;
    let ingest_summary = if ingest_ready {
        match ingest::run_loaded(loaded, pool, &ingest_args).await {
            Ok(summary) => {
                record_summary_failures(&mut stage_failures, "ingest", &summary);
                Some(summary)
            }
            Err(err) => {
                record_stage_failure(&mut stage_failures, "ingest", &err);
                None
            }
        }
    } else {
        None
    };

    // ai-run and publish run whatever ingest did: earlier runs may have left
    // stored articles to score and reports to publish (e.g. stored_local
    // records), and an ingest failure must not hold those back.
    let ai_run_summary = if !ai_enabled {
        // ai.enabled=false: skip ai-run with one INFO line and no
        // StageFailure (the standalone ai-run exit-78 contract does
        // not apply when run orchestrates it implicitly).
        tracing::info!(
            stage = "ai-run",
            reason = AI_RUN_SKIP_REASON_DISABLED,
            "AI disabled (ai.enabled=false), skipping ai-run"
        );
        ai_run_skip_reason = Some(AI_RUN_SKIP_REASON_DISABLED);
        None
    } else {
        // Every selected category, each with its own credentials.
        let categories = loaded.categories_filtered().cloned().collect::<Vec<_>>();
        match ai_run::run_categories(loaded, pool, &categories, &ai_args).await {
            Ok(summary) => {
                record_summary_failures(&mut stage_failures, "ai-run", &summary);
                Some(summary)
            }
            Err(err) => {
                record_stage_failure(&mut stage_failures, "ai-run", &err);
                None
            }
        }
    };
    let publish_result = match publish_all::preflight(loaded, &publish_args) {
        Ok(date) => publish_all::run_loaded(loaded, pool, &publish_args, &date).await,
        Err(err) => Err(err),
    };
    let publish_summary = match publish_result {
        Ok(summary) => {
            record_summary_failures(&mut stage_failures, "publish", &summary);
            Some(summary)
        }
        Err(err) => {
            record_stage_failure(&mut stage_failures, "publish", &err);
            None
        }
    };

    Ok(RunCommandSummary {
        ingest: ingest_summary,
        ai_run: ai_run_summary,
        publish: publish_summary,
        stage_failures,
        ai_run_skip_reason,
        overall_duration_seconds: started.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(stage: &'static str, kind: &str, exit_code_value: i32) -> StageFailure {
        StageFailure {
            stage,
            error_kind: kind.to_string(),
            message: format!("simulated {stage} failure"),
            exit_code_value,
        }
    }

    fn empty_summary() -> RunCommandSummary {
        RunCommandSummary {
            ingest: None,
            ai_run: None,
            publish: None,
            stage_failures: Vec::new(),
            ai_run_skip_reason: None,
            overall_duration_seconds: 0.0,
        }
    }

    #[test]
    fn no_failures_yields_success_status_and_exit_code() {
        let summary = empty_summary();
        assert_eq!(summary.status(), "success");
        assert!(summary.errors().is_empty());
        assert_eq!(summary.derive_exit_code().as_i32(), 0);
    }

    #[test]
    fn publish_failure_propagates_to_status_and_exit_code() {
        let mut summary = empty_summary();
        summary
            .stage_failures
            .push(failure("publish", "runtime", 1));
        assert_eq!(summary.status(), "fail");
        assert_eq!(summary.errors().len(), 1);
        assert_eq!(summary.derive_exit_code().as_i32(), 1);
    }

    #[test]
    fn failed_publish_all_summary_becomes_a_stage_failure() {
        use crate::commands::publish::{StageVerdict, stage};
        use crate::commands::publish_all::PublishAllCategorySummary;

        let publish = publish_all::PublishAllCommandSummary {
            date: "2026-05-18".to_string(),
            render_version: 1,
            mode: "local".to_string(),
            categories: vec![PublishAllCategorySummary {
                category: "ai".to_string(),
                publish_record_id: 1,
                items: 1,
                local_path: None,
                commit_sha: None,
                remote_target: None,
                stages: vec![
                    stage("init", "created", StageVerdict::Ok),
                    stage(
                        "store_local",
                        "Failed { error_kind: \"local_io\" }",
                        StageVerdict::Failed,
                    ),
                ],
            }],
            commit_sha: None,
            forced: false,
        };
        assert_eq!(publish.exit_code(), ExitCode::RuntimeError);

        let mut failures = Vec::new();
        record_summary_failures(&mut failures, "publish", &publish);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].stage, "publish");
        assert_eq!(failures[0].error_kind, "publish_store_local");
        assert_eq!(failures[0].exit_code_value, 1);

        let mut summary = empty_summary();
        summary.publish = Some(publish);
        summary.stage_failures = failures;
        assert_eq!(summary.exit_code(), ExitCode::RuntimeError);
        assert_eq!(summary.status(), "fail");
    }

    #[test]
    fn successful_child_summary_records_no_failure() {
        let mut failures = Vec::new();
        record_summary_failures(&mut failures, "ai-run", &empty_summary());
        assert!(failures.is_empty());
    }

    #[test]
    fn config_failure_outranks_runtime_failure() {
        let mut summary = empty_summary();
        summary.stage_failures.push(failure("ai-run", "runtime", 1));
        summary
            .stage_failures
            .push(failure("publish", "config", 78));
        assert_eq!(summary.derive_exit_code().as_i32(), 78);
    }

    #[test]
    fn ai_run_skipped_via_disabled_carve_out_is_not_a_failure() {
        // §4.11 lines 362-368: when run skips ai-run because
        // ai.enabled=false, the summary must report "success" overall and
        // exit 0 — the carve-out specifically rejects exit 78 here.
        let mut summary = empty_summary();
        summary.ai_run_skip_reason = Some(AI_RUN_SKIP_REASON_DISABLED);
        assert_eq!(summary.status(), "success");
        assert!(summary.errors().is_empty());
        assert_eq!(summary.derive_exit_code().as_i32(), 0);
    }

    #[test]
    fn ai_run_skipped_renders_distinct_pretty_line_from_failed() {
        // The pretty output must visibly distinguish "skipped" from
        // "failed" so operators can tell whether an action is required
        // (config mismatch) or whether the carve-out fired (expected).
        let mut skipped = empty_summary();
        skipped.ai_run_skip_reason = Some(AI_RUN_SKIP_REASON_DISABLED);
        let mut buf = Vec::new();
        skipped.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("skipped (ai.enabled=false)"),
            "pretty output missing skip reason: {text}"
        );

        let failed = empty_summary();
        let mut buf = Vec::new();
        failed.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("AI:                      failed"),
            "pretty output missing failed line: {text}"
        );
    }

    #[test]
    fn ai_run_skip_reason_serializes_in_json() {
        let mut summary = empty_summary();
        summary.ai_run_skip_reason = Some(AI_RUN_SKIP_REASON_DISABLED);
        let value = serde_json::to_value(&summary).expect("serialize summary");
        assert_eq!(value["ai_run_skip_reason"], "ai.enabled=false");
    }

    #[test]
    fn errors_carry_stage_prefix_for_json_consumers() {
        let mut summary = empty_summary();
        summary
            .stage_failures
            .push(failure("publish", "runtime", 1));
        let errors = summary.errors();
        assert_eq!(errors[0].kind, "publish_runtime");
        assert!(errors[0].message.starts_with("[publish] "));
    }

    /// A minimal SQLite config whose single category reads `feed_url`.
    fn write_config(root: &std::path::Path, sqlite_path: &str, feed_url: &str, ai_enabled: bool) {
        std::fs::create_dir_all(root.join("categories")).expect("categories dir");
        let output_dir = root.join("output").to_string_lossy().replace('\\', "/");
        let artifact_dir = root.join("artifacts").to_string_lossy().replace('\\', "/");
        std::fs::write(
            root.join("app.toml"),
            format!(
                r#"
schema_version = "1"

[database]
driver = "sqlite"
sqlite_path = "{sqlite_path}"
max_connections = 1
busy_timeout_ms = 5000

[http]
user_agent = "test"
timeout_seconds = 5
max_retries = 1
retry_backoff_base_ms = 1
concurrent_feeds = 1
concurrent_fetches = 1

[ai]
enabled = {ai_enabled}
model = "test-model"
max_tokens = 1024
temperature = 0.0
request_timeout_seconds = 5
max_input_chars = 1024

[ai.rate_limit]
requests_per_minute = 60
tokens_per_minute = 0

[publish]
target_timezone = "Asia/Shanghai"
github_owner = ""
github_repo = ""
github_branch = "main"
github_path_prefix = "archive"
local_output_dir = "{output_dir}"
include_unscored = true
max_items_per_report = 30
min_importance_score = 0

[publish.template]
path_template = "{{CATEGORY_KEY}}/{{YYYY}}/{{YYYYMMDD}}.md"
frontmatter_template = "---\ntitle: {{date}}\n---\n"
report_template = "{{frontmatter}}\n# {{title_md}}\n{{items}}"
item_template = "- {{item_title_md}} {{summary_blockquote}} {{url_md}}\n"

[dedup]
enable_link_dedup = true
enable_content_dedup = true
link_normalizer_version = "1"

[extractor]
strategy_order = ["summary_fallback"]
max_body_bytes = 1048576
min_body_chars = 1

[lease]
fetch_duration_seconds = 30
ai_duration_seconds = 30
publish_duration_seconds = 30
reclaim_interval_seconds = 30

[retry]
feed_entry_max_attempts = 1
ai_max_attempts = 1
publish_max_attempts = 1

[artifact]
retention_policy = "off"
sample_rate = 1.0
inline_threshold_bytes = 65536
file_storage_dir = "{artifact_dir}"
ttl_days = 30

[observability]
log_level = "info"
log_format = "pretty"
log_file = ""
enable_metrics = false
metrics_bind = "127.0.0.1:9090"
"#
            ),
        )
        .expect("write app.toml");
        std::fs::write(
            root.join("categories").join("ai.toml"),
            format!(
                r#"
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[[sources]]
key = "src"
display_name = "Source"
feed_url = "{feed_url}"
feed_kind = "rss"
priority = 10
enabled = true
"#
            ),
        )
        .expect("write category");
    }

    /// Loads the config with no RSSHub / AI / GitHub values in the
    /// environment snapshot, whatever the host process has set.
    fn load_without_env(root: &std::path::Path) -> LoadedConfig {
        let mut loaded = config::load_skip_env_checks(root, None, config::CliOverrides::default())
            .expect("load config");
        loaded.env.rsshub_base_url = None;
        loaded.env.openai_api_key = None;
        loaded.env.openai_base_url = None;
        loaded.env.github_token = None;
        loaded.env.database_url = None;
        loaded
    }

    fn run_args() -> RunArgs {
        RunArgs {
            ingest_batch_size: None,
            ai_batch_size: None,
            publish_date: Some("2026-05-18".to_string()),
            max_batches: Some(1),
        }
    }

    fn sqlite_path(root: &std::path::Path) -> String {
        root.join("run.sqlite").to_string_lossy().replace('\\', "/")
    }

    /// An ingest preflight failure (a source uses the RSSHub placeholder but
    /// RSSHUB_BASE_URL is unset) is recorded, yet publish still runs on the
    /// stored state and the exit code still reports the ingest failure.
    #[tokio::test]
    async fn ingest_preflight_failure_still_runs_publish() {
        let temp = tempfile::tempdir().expect("temp dir");
        write_config(
            temp.path(),
            &sqlite_path(temp.path()),
            "{RSSHUB}/feed.xml",
            false,
        );
        let loaded = load_without_env(temp.path());
        assert!(
            ingest::preflight(&loaded).is_err(),
            "fixture must fail the ingest preflight"
        );

        let summary = run_loaded(&loaded, &run_args(), Instant::now())
            .await
            .expect("run summary");
        assert!(summary.ingest.is_none());
        assert_eq!(
            summary
                .stage_failures
                .iter()
                .map(|failure| failure.stage)
                .collect::<Vec<_>>(),
            vec!["ingest"],
            "{:?}",
            summary.stage_failures
        );
        assert_eq!(summary.stage_failures[0].exit_code_value, 78);
        assert_eq!(
            summary.ai_run_skip_reason,
            Some(AI_RUN_SKIP_REASON_DISABLED)
        );
        assert!(summary.publish.is_some(), "publish must still run");
        assert_eq!(summary.exit_code(), ExitCode::ConfigError);
        assert_eq!(summary.status(), "fail");
    }

    /// With AI enabled, ai-run is attempted too after the ingest failure: it
    /// either reports a summary or its own (credential) failure.
    #[tokio::test]
    async fn ingest_preflight_failure_still_attempts_ai_run() {
        let temp = tempfile::tempdir().expect("temp dir");
        write_config(
            temp.path(),
            &sqlite_path(temp.path()),
            "{RSSHUB}/feed.xml",
            true,
        );
        let loaded = load_without_env(temp.path());
        let summary = run_loaded(&loaded, &run_args(), Instant::now())
            .await
            .expect("run summary");
        assert!(summary.ai_run_skip_reason.is_none());
        assert!(
            summary.ai_run.is_some()
                || summary
                    .stage_failures
                    .iter()
                    .any(|failure| failure.stage == "ai-run"),
            "ai-run was not attempted: {:?}",
            summary.stage_failures
        );
        assert!(summary.publish.is_some(), "publish must still run");
        assert_ne!(summary.exit_code(), ExitCode::Success);
    }

    /// The database is every stage's precondition: when it cannot be opened
    /// nothing downstream runs, and the failure sets the exit code.
    #[tokio::test]
    async fn unavailable_database_skips_every_stage() {
        let temp = tempfile::tempdir().expect("temp dir");
        // A path below a regular file can never be created.
        let blocker = temp.path().join("not-a-directory");
        std::fs::write(&blocker, b"file").expect("blocker file");
        let db = blocker
            .join("run.sqlite")
            .to_string_lossy()
            .replace('\\', "/");
        write_config(temp.path(), &db, "https://example.invalid/feed.xml", true);
        let loaded = load_without_env(temp.path());
        assert!(ingest::preflight(&loaded).is_ok());

        let summary = run_loaded(&loaded, &run_args(), Instant::now())
            .await
            .expect("run summary");
        assert!(summary.ingest.is_none());
        assert!(summary.ai_run.is_none());
        assert!(summary.publish.is_none());
        assert_eq!(
            summary.ai_run_skip_reason,
            Some(AI_RUN_SKIP_REASON_NO_DATABASE)
        );
        assert_eq!(
            summary.stage_failures.len(),
            1,
            "{:?}",
            summary.stage_failures
        );
        assert_eq!(summary.stage_failures[0].stage, "ingest");
        assert_ne!(summary.exit_code(), ExitCode::Success);
    }
}
