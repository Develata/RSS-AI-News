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
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
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
}

impl CommandSummary for PublishCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Publish completed:")?;
        writeln!(writer, "  Category: {}", self.category)?;
        writeln!(writer, "  Date:     {}", self.date)?;
        writeln!(writer, "  Items:    {}", self.items)?;
        if let Some(path) = &self.local_path {
            writeln!(writer, "  Local:    {path}")?;
        }
        if let Some(commit) = &self.commit_sha {
            writeln!(writer, "  Commit:   {commit}")?;
        }
        Ok(())
    }
}

pub async fn run(cli: &Cli, args: &PublishArgs) -> Result<PublishCommandSummary, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let category = super::ai_run::select_category(cli, &categories)?;
    let date = args.date.clone().unwrap_or_else(today_utc);
    if args.date.is_some() {
        let _ = parse_date_start(args.date.as_deref())?;
    }
    let (_pool, ctx) = build_run_context("publish", &loaded).await?;
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
        ctx.rule_version_repo
            .get_or_create("render", &force_tag, "force render trace", "v1")
            .await?
    } else {
        ctx.rule_version_repo
            .active_rule_or_register("render", "default", "default render", "v1")
            .await?
    };
    let selection_policy_version = ctx
        .rule_version_repo
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
            stages.push(stage("init", "created"));
            (publish_record_id, "pending".to_string())
        }
        PublishInitOutcome::AlreadyExists {
            publish_record_id,
            state,
        } => {
            stages.push(stage("init", &format!("already_exists:{state}")));
            (publish_record_id, state)
        }
    };
    if state == "failed" {
        return Err(CliError::PublishConflict { state });
    }

    let display_name = category.category.display_name.clone();
    let title = format!("{display_name} 日报 {date}");
    let generated_at = OffsetDateTime::now_utc();
    let mut items = 0;
    let mut local_path = None;
    let mut commit_sha = None;
    let mut remote_target = None;

    if matches!(state.as_str(), "pending") {
        let effective = loaded
            .effective_for_category(&category.category.key)
            .ok_or_else(|| {
                CliError::Runtime(RuntimeError::Config(format!(
                    "category {} not found in loaded config",
                    category.category.key
                )))
            })?;
        let freeze = flow
            .freeze(PublishFreezeOptions {
                category_key: category.category.key.clone(),
                max_items: effective.max_items_per_report,
                min_importance_score: effective.min_importance_score,
                include_unscored: effective.include_unscored,
                ai_enabled: effective.ai_enabled,
                excerpt_max_chars: 240,
            })
            .await;
        stages.push(stage("freeze", &format!("{:?}", freeze.status)));
        items = freeze.item_count;
        if !matches!(freeze.status, PublishFreezeStatus::Frozen) {
            return Ok(summary(
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
            ));
        }
    }
    if matches!(state.as_str(), "pending" | "snapshot_frozen") {
        let render = flow
            .render(PublishRenderOptions {
                category_display_name: display_name.clone(),
                report_title: title.clone(),
                generated_at,
            })
            .await;
        stages.push(stage("render", &format!("{:?}", render.status)));
        if !matches!(render.status, PublishRenderStatus::Rendered) {
            return Ok(summary(
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
            ));
        }
    }
    if matches!(state.as_str(), "pending" | "snapshot_frozen" | "rendered") {
        let store = flow
            .store_local(PublishStoreLocalOptions {
                category_display_name: display_name.clone(),
                report_title: title.clone(),
                generated_at,
            })
            .await;
        stages.push(stage("store_local", &format!("{:?}", store.status)));
        items = items.max(store.item_count);
        local_path = store.local_path;
        if !matches!(
            store.status,
            PublishStoreLocalStatus::StoredLocal | PublishStoreLocalStatus::PublishedLocal
        ) {
            return Ok(summary(
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
            ));
        }
    }
    if mode == "remote"
        && matches!(
            state.as_str(),
            "pending" | "snapshot_frozen" | "rendered" | "stored_local"
        )
    {
        let remote = flow
            .publish_remote(PublishRemoteOptions {
                category_display_name: display_name,
                report_title: title,
                generated_at,
            })
            .await;
        stages.push(stage("publish_remote", &format!("{:?}", remote.status)));
        items = items.max(remote.item_count);
        commit_sha = remote.commit_sha;
        remote_target = remote.remote_target;
        if !matches!(remote.status, PublishRemoteStatus::PublishedRemote) {
            return Ok(summary(
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
            ));
        }
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

fn stage(stage: &str, status: &str) -> PublishStageOutcome {
    PublishStageOutcome {
        stage: stage.to_string(),
        status: status.to_string(),
    }
}

fn today_utc() -> String {
    let date = OffsetDateTime::now_utc().date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}
