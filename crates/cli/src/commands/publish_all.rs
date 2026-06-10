use std::io::{self, Write};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{
    PublishFlow, PublishFreezeOptions, PublishFreezeStatus, PublishInitOptions, PublishInitOutcome,
    PublishRemoteBatchItemOptions, PublishRemoteBatchOptions, PublishRemoteStatus,
    PublishRenderOptions, PublishRenderStatus, PublishStoreLocalOptions, PublishStoreLocalStatus,
    RuntimeError,
};
use serde::Serialize;
use time::OffsetDateTime;

use crate::{
    args::{Cli, PublishArgs},
    commands::{backfill::parse_date_start, publish::PublishStageOutcome},
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct PublishAllCommandSummary {
    pub date: String,
    pub render_version: i64,
    pub mode: String,
    pub categories: Vec<PublishAllCategorySummary>,
    pub commit_sha: Option<String>,
    pub forced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublishAllCategorySummary {
    pub category: String,
    pub publish_record_id: i64,
    pub items: u32,
    pub local_path: Option<String>,
    pub commit_sha: Option<String>,
    pub remote_target: Option<String>,
    pub stages: Vec<PublishStageOutcome>,
}

impl CommandSummary for PublishAllCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Publish-all completed:")?;
        writeln!(writer, "  Date:       {}", self.date)?;
        writeln!(writer, "  Categories: {}", self.categories.len())?;
        writeln!(
            writer,
            "  Items:      {}",
            self.categories
                .iter()
                .map(|category| category.items)
                .sum::<u32>()
        )?;
        if let Some(commit) = &self.commit_sha {
            writeln!(writer, "  Commit:     {commit}")?;
        }
        Ok(())
    }
}

pub async fn run(cli: &Cli, args: &PublishArgs) -> Result<PublishAllCommandSummary, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories = loaded
        .categories_filtered()
        .cloned()
        .collect::<Vec<CategoryConfig>>();
    if categories.is_empty() {
        return Err(CliError::Runtime(RuntimeError::Config(
            "no categories selected".to_string(),
        )));
    }

    let date = args.date.clone().unwrap_or_else(today_utc);
    if args.date.is_some() {
        let _ = parse_date_start(args.date.as_deref())?;
    }
    let ctx = build_run_context("publish", &loaded, None).await?;
    let flow = PublishFlow::new(ctx.clone());
    let mode = if args.local_only || ctx.publish_target_remote.is_none() {
        "local"
    } else {
        "remote"
    };
    let render_version = if args.force {
        let force_tag = format!(
            "force-all-{}-{}",
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

    let mut summaries = Vec::with_capacity(categories.len());
    let mut remote_items = Vec::new();
    let generated_at = OffsetDateTime::now_utc();

    for category in &categories {
        let mut stages = Vec::new();
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
        let mut items = 0;
        let mut local_path = None;

        if matches!(state.as_str(), "pending") {
            let freeze = flow
                .freeze_record(
                    publish_record_id,
                    PublishFreezeOptions {
                        category_key: category.category.key.clone(),
                        max_items: effective.max_items_per_report,
                        min_importance_score: effective.min_importance_score,
                        include_unscored: effective.include_unscored,
                        ai_enabled: effective.ai_enabled,
                        candidate_window_hours: loaded.app.publish.candidate_window_hours,
                        excerpt_max_chars: 240,
                    },
                )
                .await;
            stages.push(stage("freeze", &format!("{:?}", freeze.status)));
            items = freeze.item_count;
            if !matches!(freeze.status, PublishFreezeStatus::Frozen) {
                summaries.push(category_summary(
                    category,
                    publish_record_id,
                    items,
                    local_path,
                    None,
                    None,
                    stages,
                ));
                continue;
            }
        }
        if matches!(state.as_str(), "pending" | "snapshot_frozen") {
            let render = flow
                .render_record(
                    publish_record_id,
                    PublishRenderOptions {
                        category_display_name: display_name.clone(),
                        report_title: title.clone(),
                        generated_at,
                        path_template: path_template.clone(),
                    },
                )
                .await;
            stages.push(stage("render", &format!("{:?}", render.status)));
            if !matches!(render.status, PublishRenderStatus::Rendered) {
                summaries.push(category_summary(
                    category,
                    publish_record_id,
                    items,
                    local_path,
                    None,
                    None,
                    stages,
                ));
                continue;
            }
        }
        if matches!(state.as_str(), "pending" | "snapshot_frozen" | "rendered") {
            let store = flow
                .store_local_record(
                    publish_record_id,
                    PublishStoreLocalOptions {
                        category_display_name: display_name.clone(),
                        report_title: title.clone(),
                        generated_at,
                        path_template: path_template.clone(),
                    },
                )
                .await;
            stages.push(stage("store_local", &format!("{:?}", store.status)));
            items = items.max(store.item_count);
            local_path = store.local_path;
            if !matches!(
                store.status,
                PublishStoreLocalStatus::StoredLocal | PublishStoreLocalStatus::PublishedLocal
            ) {
                summaries.push(category_summary(
                    category,
                    publish_record_id,
                    items,
                    local_path,
                    None,
                    None,
                    stages,
                ));
                continue;
            }
        }

        if mode == "remote"
            && matches!(
                state.as_str(),
                "pending" | "snapshot_frozen" | "rendered" | "stored_local"
            )
        {
            remote_items.push(PublishRemoteBatchItemOptions {
                publish_record_id,
                category_display_name: display_name,
                report_title: title,
                generated_at,
                path_template,
            });
        }
        summaries.push(category_summary(
            category,
            publish_record_id,
            items,
            local_path,
            None,
            None,
            stages,
        ));
    }

    let mut commit_sha = None;
    if mode == "remote" && !remote_items.is_empty() {
        let batch = flow
            .publish_remote_batch(PublishRemoteBatchOptions {
                items: remote_items,
            })
            .await;
        commit_sha = batch.commit_sha;
        for remote in batch.items {
            if let Some(summary) = summaries
                .iter_mut()
                .find(|summary| summary.publish_record_id == remote.publish_record_id)
            {
                summary.stages.push(stage(
                    "publish_remote_batch",
                    &format!("{:?}", remote.status),
                ));
                summary.items = summary.items.max(remote.item_count);
                summary.commit_sha = remote.commit_sha;
                summary.remote_target = remote.remote_target;
            }
            if !matches!(
                remote.status,
                PublishRemoteStatus::PublishedRemote | PublishRemoteStatus::NothingToClaim
            ) {
                // 保持与单 category publish 相同风格：流程结果进 summary，
                // 是否重试由状态机和下一轮调度决定。
                tracing::warn!(
                    publish_record_id = remote.publish_record_id,
                    status = ?remote.status,
                    "remote batch publish item did not reach published_remote"
                );
            }
        }
    }

    Ok(PublishAllCommandSummary {
        date,
        render_version,
        mode: mode.to_string(),
        categories: summaries,
        commit_sha,
        forced: args.force,
    })
}

#[allow(clippy::too_many_arguments)]
fn category_summary(
    category: &CategoryConfig,
    publish_record_id: i64,
    items: u32,
    local_path: Option<String>,
    commit_sha: Option<String>,
    remote_target: Option<String>,
    stages: Vec<PublishStageOutcome>,
) -> PublishAllCategorySummary {
    PublishAllCategorySummary {
        category: category.category.key.clone(),
        publish_record_id,
        items,
        local_path,
        commit_sha,
        remote_target,
        stages,
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
