use rss_ai_news_storage::RuleVersionRepository;
use std::io::{self, Write};

use rss_ai_news_config::{self as config, CategoryConfig, LoadedConfig};
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_runtime::{
    PublishFlow, PublishFreezeOptions, PublishInitOptions, PublishInitOutcome,
    PublishRemoteBatchItemOptions, PublishRemoteBatchOptions, PublishRenderOptions, RuntimeError,
};
use rss_ai_news_storage::StoragePool;
use serde::Serialize;
use time::OffsetDateTime;

use crate::{
    args::{Cli, PublishArgs},
    commands::{
        backfill::parse_date_start,
        publish::{
            PublishStageOutcome, StageVerdict, run_local_stages, stage, stage_errors, stage_trail,
            stages_exit_code, today_utc,
        },
    },
    context_factory::{build_publish_deps, open_write_storage},
    error::CliError,
    exit_code::ExitCode,
    output::{CommandSummary, RenderedError},
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
    fn exit_code(&self) -> ExitCode {
        stages_exit_code(self.categories.iter().flat_map(|category| &category.stages))
    }

    fn errors(&self) -> Vec<RenderedError> {
        self.categories
            .iter()
            .flat_map(|category| stage_errors(&category.category, &category.stages))
            .collect()
    }

    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        if self.exit_code() == ExitCode::Success {
            writeln!(writer, "Publish-all completed:")?;
        } else {
            writeln!(writer, "Publish-all failed:")?;
        }
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
        for category in &self.categories {
            writeln!(
                writer,
                "  - {:<12} {:>3} items  {}",
                category.category,
                category.items,
                stage_trail(&category.stages)
            )?;
            if let Some(path) = &category.local_path {
                writeln!(writer, "    {path}")?;
            }
        }
        Ok(())
    }
}

pub async fn run(cli: &Cli, args: &PublishArgs) -> Result<PublishAllCommandSummary, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    preflight(&loaded, args)?;
    let pool = open_write_storage(&loaded).await?;
    run_loaded(&loaded, &pool, args).await
}

/// Config / argument checks for publish-all; run before storage is opened.
pub(crate) fn preflight(loaded: &LoadedConfig, args: &PublishArgs) -> Result<(), CliError> {
    config::validate::run_command_checks(
        loaded,
        config::validate::CommandKind::Publish,
        &config::validate::CommandFlags {
            local_only: args.local_only,
        },
    )?;
    if loaded.categories_filtered().next().is_none() {
        return Err(CliError::Runtime(RuntimeError::Config(
            "no categories selected".to_string(),
        )));
    }
    if args.date.is_some() {
        let _ = parse_date_start(args.date.as_deref())?;
    }
    Ok(())
}

/// Publishes every selected category on an already loaded config and opened
/// pool; callers run [`preflight`] first.
pub(crate) async fn run_loaded(
    loaded: &LoadedConfig,
    pool: &StoragePool,
    args: &PublishArgs,
) -> Result<PublishAllCommandSummary, CliError> {
    let categories = loaded
        .categories_filtered()
        .cloned()
        .collect::<Vec<CategoryConfig>>();
    let date = args.date.clone().unwrap_or_else(today_utc);
    let ctx = build_publish_deps(loaded, pool, args.local_only)?;
    let rule_version_repo = rss_ai_news_storage::RuleVersionRepo::new_with_storage(pool.clone());
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

    let mut summaries = Vec::with_capacity(categories.len());
    let mut remote_items = Vec::new();
    let generated_at = OffsetDateTime::now_utc();

    // A category-level init failure or conflict is recorded in that category's
    // summary (verdict failed → exit 1) instead of aborting: categories are
    // independent, and results already stored locally must still reach the
    // remote batch below.
    for category in &categories {
        let mut stages = Vec::new();
        let init = match flow
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
            .await
        {
            Ok(init) => init,
            Err(error) => {
                tracing::error!(
                    category_key = %category.category.key,
                    "publish init failed: {error}"
                );
                stages.push(stage(
                    "init",
                    &format!("error:{}", error.error_kind()),
                    StageVerdict::Failed,
                ));
                summaries.push(category_summary(category, 0, 0, None, None, None, stages));
                continue;
            }
        };
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
            // Terminal record for this (category, date); `--force` re-publishes.
            stages.push(stage("conflict", "record_failed", StageVerdict::Failed));
            summaries.push(category_summary(
                category,
                publish_record_id,
                0,
                None,
                None,
                None,
                stages,
            ));
            continue;
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
        let items = local.items;
        let local_path = local.local_path;
        if !local.completed {
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
                    StageVerdict::of_remote(&remote.status),
                ));
                summary.items = summary.items.max(remote.item_count);
                summary.commit_sha = remote.commit_sha;
                summary.remote_target = remote.remote_target;
            }
            if StageVerdict::of_remote(&remote.status) == StageVerdict::Failed {
                // 结果进 summary（verdict=failed → exit 1）；是否重试由状态机
                // 和下一轮调度决定。
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
