use std::{
    io::{self, Write},
    time::Instant,
};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{AiRunFlow, AiRunOptions, RuntimeError, ai_lease_budget_seconds};
use serde::Serialize;

use crate::{
    args::{AiRunArgs, Cli},
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct AiRunCommandSummary {
    pub task_gen_scanned: u32,
    pub task_gen_inserted: u32,
    pub task_gen_conflict_skipped: u32,
    pub process_claimed: u32,
    pub process_succeeded: u32,
    pub process_filtered: u32,
    pub process_retryable_failed: u32,
    pub process_permanent_failed: u32,
    /// 因 task panic / cancel 而失败的 AI 任务数（codex P2-1）。与业务永久失败
    /// 分列，避免 panic 在运维输出中报 0 failure。
    pub process_tasks_panicked: u32,
    pub duration_seconds: f64,
}

impl CommandSummary for AiRunCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "AI run completed:")?;
        writeln!(writer, "  Task-gen scanned:      {}", self.task_gen_scanned)?;
        writeln!(
            writer,
            "  Task-gen inserted:     {}",
            self.task_gen_inserted
        )?;
        writeln!(
            writer,
            "  Task-gen conflicts:    {}",
            self.task_gen_conflict_skipped
        )?;
        writeln!(writer, "  Claimed:               {}", self.process_claimed)?;
        writeln!(
            writer,
            "  Succeeded:             {}",
            self.process_succeeded
        )?;
        writeln!(writer, "  Filtered:              {}", self.process_filtered)?;
        writeln!(
            writer,
            "  Failed (retryable):    {}",
            self.process_retryable_failed
        )?;
        writeln!(
            writer,
            "  Failed (permanent):    {}",
            self.process_permanent_failed
        )?;
        writeln!(
            writer,
            "  Tasks panicked:        {}",
            self.process_tasks_panicked
        )?;
        writeln!(
            writer,
            "  Duration:              {:.2}s",
            self.duration_seconds
        )
    }
}

pub async fn run(cli: &Cli, args: &AiRunArgs) -> Result<AiRunCommandSummary, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let category = select_category(cli, &categories)?;
    // W14-B：按选定板块解析有效凭证（override 非空 > 全局 env），缺失即
    // fail-fast（错误只含 env 变量名），单 client 静态装配。
    let ai_credentials = loaded.ai_credentials_for_category(&category.category.key)?;
    let started = Instant::now();
    let ctx = build_run_context("ai-run", &loaded, Some(ai_credentials)).await?;

    // F15-3: 生产读路径走 active_rule_or_register（先读 active，无则 seed
    // 首版）。直接 get_or_create 会被 partial unique index 误判（同 kind
    // 仅一行 active），导致 reindex 切换后无法继续 ingest。
    let prompt_version = ctx
        .rule_version_repo
        .active_rule_or_register("prompt", "default", "default prompt version", "0")
        .await?;
    let output_schema_version = ctx
        .rule_version_repo
        .active_rule_or_register("ai_output_schema", "v1", "AI v1 schema", "v1")
        .await?;
    let prompt_template = category
        .ai_override
        .as_ref()
        .and_then(|override_| override_.prompt_template.clone())
        .unwrap_or_else(|| "Summarize the following article.".to_string());
    // Threshold inheritance must match the publish stage exactly: F4 audit
    // found that an ad-hoc `unwrap_or(50)` here diverged from the global
    // default (30) used by `effective_for_category`. Routing through a single
    // truth source guarantees AI filtering and publish selection see the
    // same `min_importance_score`. See docs/design/config-schema.md §4.5.
    let effective = loaded
        .effective_for_category(&category.category.key)
        .ok_or_else(|| {
            CliError::Runtime(RuntimeError::Config(format!(
                "category {} not found in loaded config",
                category.category.key
            )))
        })?;
    // F6-1: `Score0To100` 直接传给 `AiRunOptions`，与 publish 路径
    // (`PublishFreezeOptions.min_importance_score`) 类型契约对齐；
    // 不在 CLI 层提前 `.get() as i32` 失去 newtype 不变量。
    let min_importance_score = effective.min_importance_score;
    // W14-A：主模型走 effective（trim 后 category > global）再让 CLI --model 覆盖，修复
    // 直接用 ai_override.model 会把示例里 model="" 当真实模型的缺陷（codex P3 HIGH）。
    let model_id = args
        .model
        .clone()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| effective.model.clone());
    let fallback_models = effective.fallback_models.clone();
    // W14-A：运行前 lease 预算 fail-fast（含批排队 + fallback 链长，codex P3 BLOCKER）。
    let lease_budget = ai_lease_budget_seconds(
        args.batch_size,
        loaded.app.http.concurrent_fetches,
        loaded.app.ai.request_timeout_seconds,
        fallback_models.len(),
    );
    if lease_budget > loaded.app.lease.ai_duration_seconds {
        return Err(CliError::Runtime(RuntimeError::Config(format!(
            "AI lease 预算 {lease_budget}s（ceil(batch {} / concurrent_fetches {}) × \
             request_timeout {}s × (1 + fallback {})）超过 lease.ai_duration_seconds {}s；\
             请减小 --batch-size、缩短 fallback 链，或调大 lease.ai_duration_seconds",
            args.batch_size,
            loaded.app.http.concurrent_fetches,
            loaded.app.ai.request_timeout_seconds,
            fallback_models.len(),
            loaded.app.lease.ai_duration_seconds,
        ))));
    }
    let max_input_chars = category
        .ai_override
        .as_ref()
        .and_then(|override_| override_.max_input_chars)
        .unwrap_or(loaded.app.ai.max_input_chars);

    let summary = AiRunFlow::new(ctx)
        .run(AiRunOptions {
            task_gen_batch_size: args.batch_size,
            process_batch_size: args.batch_size,
            max_attempts: loaded.app.retry.ai_max_attempts,
            prompt_template,
            model_id,
            fallback_models,
            max_input_chars,
            max_tokens: loaded.app.ai.max_tokens,
            temperature: loaded.app.ai.temperature,
            min_importance_score,
            // F6-3: 从 app.runtime.max_batches_per_run 取生效值（CLI > config > 默认）。
            // 仅约束 process 阶段 claim 循环；task_gen 是 one-shot sweep。
            max_batches: loaded.app.runtime.max_batches_per_run,
            category_key: category.category.key.clone(),
            prompt_version,
            output_schema_version,
        })
        .await;

    Ok(AiRunCommandSummary {
        task_gen_scanned: summary.task_gen.scanned,
        task_gen_inserted: summary.task_gen.inserted,
        task_gen_conflict_skipped: summary.task_gen.conflict_skipped,
        process_claimed: summary.process.claimed,
        process_succeeded: summary.process.succeeded,
        process_filtered: summary.process.filtered,
        process_retryable_failed: summary.process.retryable_failed,
        process_permanent_failed: summary.process.permanent_failed,
        process_tasks_panicked: summary.process.tasks_panicked,
        duration_seconds: started.elapsed().as_secs_f64(),
    })
}

pub(crate) fn select_category<'a>(
    cli: &Cli,
    categories: &'a [CategoryConfig],
) -> Result<&'a CategoryConfig, CliError> {
    if let Some(key) = &cli.category {
        return categories
            .iter()
            .find(|cat| cat.category.key == *key)
            .ok_or_else(|| {
                CliError::Runtime(rss_ai_news_runtime::RuntimeError::Config(format!(
                    "category {key} not found"
                )))
            });
    }
    if categories.len() == 1 {
        return Ok(&categories[0]);
    }
    Err(CliError::Runtime(
        rss_ai_news_runtime::RuntimeError::Config(
            "category is required when multiple or zero categories are configured".to_string(),
        ),
    ))
}
