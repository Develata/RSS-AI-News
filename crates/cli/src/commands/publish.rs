use rss_ai_news_storage::RuleVersionRepository;
use std::io::{self, Write};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{
    PublishFlow, PublishFreezeOptions, PublishFreezeStatus, PublishInitOptions, PublishInitOutcome,
    PublishRemoteOptions, PublishRemoteStatus, PublishRenderOptions, PublishRenderStatus,
    PublishStoreLocalOptions, PublishStoreLocalStatus, RuntimeError,
};
use serde::Serialize;
use time::OffsetDateTime;

use crate::{
    args::{Cli, PublishArgs},
    commands::backfill::parse_date_start,
    context_factory::{build_publish_deps, open_write_storage},
    error::CliError,
    exit_code::ExitCode,
    output::{CommandSummary, RenderedError},
};

#[derive(Debug, Clone, Serialize)]
pub struct PublishCommandSummary {
    pub category: String,
    pub date: String,
    pub render_version: i64,
    pub publish_record_id: i64,
    pub mode: String,
    pub items: u32,
    pub local_path: Option<String>,
    pub commit_sha: Option<String>,
    pub remote_target: Option<String>,
    pub stages: Vec<PublishStageOutcome>,
    pub forced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublishStageOutcome {
    pub stage: String,
    pub status: String,
    pub verdict: StageVerdict,
}

/// How a publish stage outcome affects the command result
/// (docs/plan/11-error-and-recovery.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageVerdict {
    /// The stage reached its success state.
    Ok,
    /// Non-fatal: nothing to publish (`SnapshotEmpty`), or the record is
    /// leased / advanced by another worker (`NothingToClaim`, `Conflicted`).
    /// Exit 0; the next scheduled run picks it up if still pending.
    Skipped,
    /// The stage failed; the command exits 1.
    Failed,
}

impl StageVerdict {
    pub fn of_freeze(status: &PublishFreezeStatus) -> Self {
        match status {
            PublishFreezeStatus::Frozen => Self::Ok,
            PublishFreezeStatus::SnapshotEmpty
            | PublishFreezeStatus::NothingToClaim
            | PublishFreezeStatus::Conflicted => Self::Skipped,
            PublishFreezeStatus::ArticleConflict { .. } | PublishFreezeStatus::Failed { .. } => {
                Self::Failed
            }
        }
    }

    pub fn of_render(status: &PublishRenderStatus) -> Self {
        match status {
            PublishRenderStatus::Rendered => Self::Ok,
            PublishRenderStatus::NothingToClaim | PublishRenderStatus::Conflicted => Self::Skipped,
            PublishRenderStatus::Failed { .. } => Self::Failed,
        }
    }

    pub fn of_store_local(status: &PublishStoreLocalStatus) -> Self {
        match status {
            PublishStoreLocalStatus::StoredLocal | PublishStoreLocalStatus::PublishedLocal => {
                Self::Ok
            }
            PublishStoreLocalStatus::NothingToClaim | PublishStoreLocalStatus::Conflicted => {
                Self::Skipped
            }
            PublishStoreLocalStatus::ArticleConflict { .. }
            | PublishStoreLocalStatus::Failed { .. } => Self::Failed,
        }
    }

    pub fn of_remote(status: &PublishRemoteStatus) -> Self {
        match status {
            PublishRemoteStatus::PublishedRemote => Self::Ok,
            PublishRemoteStatus::NothingToClaim | PublishRemoteStatus::Conflicted => Self::Skipped,
            PublishRemoteStatus::ArticleConflict { .. }
            | PublishRemoteStatus::MissingTarget
            | PublishRemoteStatus::Failed { .. } => Self::Failed,
        }
    }
}

/// Exit code for a set of stage outcomes: any failed stage → runtime error.
pub(crate) fn stages_exit_code<'a>(
    stages: impl IntoIterator<Item = &'a PublishStageOutcome>,
) -> ExitCode {
    if stages
        .into_iter()
        .any(|stage| stage.verdict == StageVerdict::Failed)
    {
        ExitCode::RuntimeError
    } else {
        ExitCode::Success
    }
}

/// Failed stages rendered for the JSON envelope's `errors` array.
pub(crate) fn stage_errors<'a>(
    category: &str,
    stages: impl IntoIterator<Item = &'a PublishStageOutcome>,
) -> Vec<RenderedError> {
    stages
        .into_iter()
        .filter(|stage| stage.verdict == StageVerdict::Failed)
        .map(|stage| RenderedError {
            kind: format!("publish_{}", stage.stage),
            message: format!("[{category}] {} {}", stage.stage, stage.status),
        })
        .collect()
}

/// `init=created → freeze=Frozen → …`, the stage trail shown in pretty output.
pub(crate) fn stage_trail(stages: &[PublishStageOutcome]) -> String {
    stages
        .iter()
        .map(|stage| format!("{}={}", stage.stage, stage.status))
        .collect::<Vec<_>>()
        .join(" → ")
}

impl CommandSummary for PublishCommandSummary {
    fn exit_code(&self) -> ExitCode {
        stages_exit_code(&self.stages)
    }

    fn errors(&self) -> Vec<RenderedError> {
        stage_errors(&self.category, &self.stages)
    }

    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        if self.exit_code() == ExitCode::Success {
            writeln!(writer, "Publish completed:")?;
        } else {
            writeln!(writer, "Publish failed:")?;
        }
        writeln!(writer, "  Category: {}", self.category)?;
        writeln!(writer, "  Date:     {}", self.date)?;
        writeln!(writer, "  Items:    {}", self.items)?;
        if let Some(path) = &self.local_path {
            writeln!(writer, "  Local:    {path}")?;
        }
        if let Some(commit) = &self.commit_sha {
            writeln!(writer, "  Commit:   {commit}")?;
        }
        writeln!(writer, "  Stages:   {}", stage_trail(&self.stages))?;
        Ok(())
    }
}

pub async fn run(cli: &Cli, args: &PublishArgs) -> Result<PublishCommandSummary, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    config::validate::run_command_checks(
        &loaded,
        config::validate::CommandKind::Publish,
        &config::validate::CommandFlags {
            local_only: args.local_only,
        },
    )?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let category = super::ai_run::select_category(cli, &categories)?;
    let date = args.date.clone().unwrap_or_else(today_utc);
    if args.date.is_some() {
        let _ = parse_date_start(args.date.as_deref())?;
    }
    let pool = open_write_storage(&loaded).await?;
    let ctx = build_publish_deps(&loaded, &pool, args.local_only)?;
    let rule_version_repo = rss_ai_news_storage::RuleVersionRepo::new_with_storage(pool.clone());
    let flow = PublishFlow::new(ctx.clone());
    let mode = if args.local_only || ctx.publish_target_remote.is_none() {
        "local"
    } else {
        "remote"
    };
    // F15-3: --force 显式制造一条新的 render rule_version 行用于 audit
    // trace；非 force 分支走 active_rule_or_register 读路径。force 行落
    // 在 partial unique 之外（status='pending'，由 get_or_create CASE
    // 自动选择，因为同 kind 已有 active 行）。
    let render_version = if args.force {
        let force_tag = format!(
            "force-{}-{}-{}",
            category.category.key,
            date,
            OffsetDateTime::now_utc().unix_timestamp()
        );
        rule_version_repo
            .get_or_create("render", &force_tag, "force render trace", "v1")
            .await?
    } else {
        rule_version_repo
            .active_rule_or_register("render", "default", "default render", "v1")
            .await?
    };
    let selection_policy_version = rule_version_repo
        .active_rule_or_register(
            "selection_policy",
            "default",
            "default selection policy",
            "v1",
        )
        .await?;
    let init = flow
        .init(PublishInitOptions {
            category_key: category.category.key.clone(),
            report_date: date.clone(),
            target_timezone: loaded.app.publish.target_timezone.clone(),
            render_version,
            selection_policy_version,
            remote_target: (mode == "remote").then(|| {
                format!(
                    "{}/{}:{}",
                    loaded.app.publish.github_owner,
                    loaded.app.publish.github_repo,
                    loaded.app.publish.github_branch
                )
            }),
        })
        .await?;

    let mut stages = Vec::new();
    let (publish_record_id, state) = match init {
        PublishInitOutcome::Created { publish_record_id } => {
            stages.push(stage("init", "created", StageVerdict::Ok));
            (publish_record_id, "pending".to_string())
        }
        PublishInitOutcome::AlreadyExists {
            publish_record_id,
            state,
        } => {
            stages.push(stage(
                "init",
                &format!("already_exists:{state}"),
                StageVerdict::Ok,
            ));
            (publish_record_id, state)
        }
    };
    if state == "failed" {
        return Err(CliError::PublishConflict { state });
    }

    let display_name = category.category.display_name.clone();
    let title = format!("{display_name} {date}");
    let effective = loaded
        .effective_for_category(&category.category.key)
        .ok_or_else(|| {
            CliError::Runtime(RuntimeError::Config(format!(
                "category {} not found in loaded config",
                category.category.key
            )))
        })?;
    let path_template = Some(effective.path_template.clone());
    let generated_at = OffsetDateTime::now_utc();
    let local = run_local_stages(
        &flow,
        publish_record_id,
        &state,
        PublishFreezeOptions {
            category_key: category.category.key.clone(),
            max_items: effective.max_items_per_report,
            min_importance_score: effective.min_importance_score,
            include_unscored: effective.include_unscored,
            ai_enabled: effective.ai_enabled,
            candidate_window_hours: loaded.app.publish.candidate_window_hours,
            excerpt_max_chars: 240,
        },
        PublishRenderOptions {
            category_display_name: display_name.clone(),
            report_title: title.clone(),
            generated_at,
            path_template: path_template.clone(),
        },
        &mut stages,
    )
    .await;
    let mut items = local.items;
    let local_path = local.local_path;
    let mut commit_sha = None;
    let mut remote_target = None;

    if local.completed
        && mode == "remote"
        && matches!(
            state.as_str(),
            "pending" | "snapshot_frozen" | "rendered" | "stored_local"
        )
    {
        let remote = flow
            .publish_remote_record(
                publish_record_id,
                PublishRemoteOptions {
                    category_display_name: display_name,
                    report_title: title,
                    generated_at,
                    path_template,
                },
            )
            .await;
        stages.push(stage(
            "publish_remote",
            &format!("{:?}", remote.status),
            StageVerdict::of_remote(&remote.status),
        ));
        items = items.max(remote.item_count);
        commit_sha = remote.commit_sha;
        remote_target = remote.remote_target;
    }

    Ok(summary(
        category,
        date,
        render_version,
        publish_record_id,
        mode,
        items,
        local_path,
        commit_sha,
        remote_target,
        stages,
        args.force,
    ))
}

/// Result of the local publish stages (freeze → render → store_local) for one
/// category.
pub(crate) struct LocalStages {
    pub items: u32,
    pub local_path: Option<String>,
    /// `true` iff every stage required by the starting `state` reached its
    /// success status, so the record may continue to remote publish.
    pub completed: bool,
}

/// Drives the local stages for exactly one `publish_record_id`, starting from
/// the record's persisted `state`. Every claim is bound to that id, so a
/// category run can never claim or advance another category's record.
/// Stage outcomes are appended to `stages`.
pub(crate) async fn run_local_stages(
    flow: &PublishFlow,
    publish_record_id: i64,
    state: &str,
    freeze: PublishFreezeOptions,
    render: PublishRenderOptions,
    stages: &mut Vec<PublishStageOutcome>,
) -> LocalStages {
    let mut outcome = LocalStages {
        items: 0,
        local_path: None,
        completed: false,
    };
    if state == "pending" {
        let freeze = flow.freeze_record(publish_record_id, freeze).await;
        stages.push(stage(
            "freeze",
            &format!("{:?}", freeze.status),
            StageVerdict::of_freeze(&freeze.status),
        ));
        outcome.items = freeze.item_count;
        if StageVerdict::of_freeze(&freeze.status) != StageVerdict::Ok {
            return outcome;
        }
    }
    if matches!(state, "pending" | "snapshot_frozen") {
        let rendered = flow.render_record(publish_record_id, render.clone()).await;
        stages.push(stage(
            "render",
            &format!("{:?}", rendered.status),
            StageVerdict::of_render(&rendered.status),
        ));
        if StageVerdict::of_render(&rendered.status) != StageVerdict::Ok {
            return outcome;
        }
    }
    if matches!(state, "pending" | "snapshot_frozen" | "rendered") {
        let store = flow
            .store_local_record(
                publish_record_id,
                PublishStoreLocalOptions {
                    category_display_name: render.category_display_name,
                    report_title: render.report_title,
                    generated_at: render.generated_at,
                    path_template: render.path_template,
                },
            )
            .await;
        stages.push(stage(
            "store_local",
            &format!("{:?}", store.status),
            StageVerdict::of_store_local(&store.status),
        ));
        outcome.items = outcome.items.max(store.item_count);
        outcome.local_path = store.local_path;
        if StageVerdict::of_store_local(&store.status) != StageVerdict::Ok {
            return outcome;
        }
    }
    outcome.completed = true;
    outcome
}

#[allow(clippy::too_many_arguments)]
fn summary(
    category: &CategoryConfig,
    date: String,
    render_version: i64,
    publish_record_id: i64,
    mode: &str,
    items: u32,
    local_path: Option<String>,
    commit_sha: Option<String>,
    remote_target: Option<String>,
    stages: Vec<PublishStageOutcome>,
    forced: bool,
) -> PublishCommandSummary {
    PublishCommandSummary {
        category: category.category.key.clone(),
        date,
        render_version,
        publish_record_id,
        mode: mode.to_string(),
        items,
        local_path,
        commit_sha,
        remote_target,
        stages,
        forced,
    }
}

pub(crate) fn stage(stage: &str, status: &str, verdict: StageVerdict) -> PublishStageOutcome {
    PublishStageOutcome {
        stage: stage.to_string(),
        status: status.to_string(),
        verdict,
    }
}

pub(crate) fn today_utc() -> String {
    let date = OffsetDateTime::now_utc().date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}
