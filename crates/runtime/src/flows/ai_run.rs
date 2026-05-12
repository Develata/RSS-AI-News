use std::sync::Arc;

use rss_ai_news_ai::{AiError, AiResponse, AiTask, ParsedResponse, parse_response};
use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_storage::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ClaimRequest, ClaimedAiResult, NewAiResult,
    ReleaseSuccessOutcome, build_owner_id, lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::artifact::ArtifactWriter;
use crate::context::RunContext;
use crate::events::RunEventEmitter;

#[derive(Debug, Clone)]
pub struct AiRunOptions {
    /// task_gen 阶段一次扫描多少条 persisted article。
    pub task_gen_batch_size: u32,
    /// process 阶段一次 claim 多少条 pending AI 任务。
    pub process_batch_size: u32,
    pub max_attempts: u32,
    pub prompt_template: String,
    pub model_id: String,
    pub max_input_chars: u32,
    pub max_tokens: u32,
    pub temperature: f32,
    /// 0..=100 的发布门槛。type-level invariant 在反序列化 / CLI 解析时已被
    /// `Score0To100` 锁死（F5-4），ai-run 路径同样使用 newtype 而不在中途
    /// 退化为 `i32`，与 publish / config 两侧保持类型契约一致（F6-1）。
    /// 调用 storage 层时按需 `.get() as i32`（SQL 绑定边界）。
    pub min_importance_score: Score0To100,
    /// 单次 run 内部 claim 循环上限。`0` = 不限。由 CLI 从
    /// `app.runtime.max_batches_per_run` 传入（F6-3）。仅约束 process 阶段
    /// 的 claim 循环；task_gen 阶段是 one-shot insert-pending sweep，不受
    /// 此上限控制。详见 docs/design/config-schema.md §4.4。
    pub max_batches: u32,
    pub category_key: String,
    /// `rule_versions` 表中 `kind='prompt'` 的当前活跃版本号。CLI 在调用前通过
    /// `rule_version_repo.get_or_create("prompt", ...)` 解析，写入
    /// `article_ai_results.prompt_version`（详见 storage-schema §4.6 幂等四元组）。
    pub prompt_version: i64,
    /// `rule_versions` 表中 `kind='ai_output_schema'` 的当前活跃版本号。CLI
    /// 在调用前通过 `rule_version_repo.get_or_create("ai_output_schema", ...)`
    /// 解析，写入 `article_ai_results.output_schema_version`。
    pub output_schema_version: i64,
}

#[derive(Debug, Default, Clone)]
pub struct AiRunSummary {
    pub task_gen: TaskGenSummary,
    pub process: AiProcessSummary,
}

#[derive(Debug, Default, Clone)]
pub struct TaskGenSummary {
    pub scanned: u32,
    pub inserted: u32,
    pub conflict_skipped: u32,
    pub article_already_advanced: u32,
}

#[derive(Debug, Default, Clone)]
pub struct AiProcessSummary {
    pub claimed: u32,
    pub succeeded: u32,
    pub filtered: u32,
    pub retryable_failed: u32,
    pub permanent_failed: u32,
    /// 实际执行的批次数（F6-3）。命中 `max_batches` 时等于上限；否则小于上限。
    pub batches_executed: u32,
    /// `true` 表示循环因 `max_batches` 上限退出（仍有 pending 任务）；
    /// `false` 表示自然耗尽（claim 返回空批次）或因 retryable 失败 defer。
    /// 与 `retryable_deferred` 互斥（同一 run 内最多一个为 `true`）。
    pub max_batches_reached: bool,
    /// `true` 表示循环因本批次出现 RetryableFailed 主动 defer 到下次 run
    /// （F6-3 retryable-bail 路径；W4-1）。三值组合 `(max_batches_reached,
    /// retryable_deferred)` = `(T, F)` / `(F, T)` / `(F, F)` 区分 cap-hit /
    /// retryable-deferred / queue-exhausted 三种退出路径。
    pub retryable_deferred: bool,
    pub per_task: Vec<AiTaskOutcome>,
}

#[derive(Debug, Clone)]
pub struct AiTaskOutcome {
    pub article_ai_result_id: i64,
    pub article_id: i64,
    pub status: AiTaskStatus,
    pub article_advance: Option<AiCompleteArticleAdvance>,
    pub error_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiTaskStatus {
    Succeeded,
    Filtered,
    RetryableFailed,
    PermanentFailed,
}

pub struct AiRunFlow {
    ctx: Arc<RunContext>,
}

impl AiRunFlow {
    pub fn new(ctx: Arc<RunContext>) -> Self {
        Self { ctx }
    }

    /// Phase 1：扫描一页 `articles.state='persisted'` 并创建 pending AI 任务。
    pub async fn task_gen(&self, opts: &AiRunOptions) -> TaskGenSummary {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "ai_run",
            repo: self.ctx.event_repo.as_ref(),
        };
        emitter
            .emit(
                "run_started",
                "info",
                None,
                None,
                "ai task_gen run started",
                Some(json!({ "phase": "task_gen" })),
            )
            .await;

        let candidates = match self
            .ctx
            .article_repo
            .list_persisted_for_ai_task_gen(opts.task_gen_batch_size.max(1), 0)
            .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::error!("failed to list AI task candidates: {error}");
                emitter
                    .emit(
                        "run_completed",
                        "info",
                        None,
                        None,
                        "ai task_gen run completed",
                        Some(json!({
                            "phase": "task_gen",
                            "scanned": 0,
                            "error_kind": error.error_kind(),
                        })),
                    )
                    .await;
                return TaskGenSummary::default();
            }
        };

        let mut summary = TaskGenSummary {
            scanned: candidates.len() as u32,
            ..TaskGenSummary::default()
        };

        for candidate in candidates {
            let item = NewAiResult {
                article_id: candidate.article_id,
                prompt_version: opts.prompt_version,
                output_schema_version: opts.output_schema_version,
                model_id: opts.model_id.clone(),
            };
            match self
                .ctx
                .ai_result_repo
                .insert_pending_and_advance_article(&item, OffsetDateTime::now_utc())
                .await
            {
                Ok(outcome) => {
                    if outcome.ai_result_id.is_some() {
                        summary.inserted += 1;
                    } else {
                        summary.conflict_skipped += 1;
                    }
                    if outcome.article_already_advanced {
                        summary.article_already_advanced += 1;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        article_id = candidate.article_id,
                        "failed to insert AI pending result: {error}"
                    );
                }
            }
        }

        emitter
            .emit(
                "run_completed",
                "info",
                None,
                None,
                "ai task_gen run completed",
                Some(json!({
                    "phase": "task_gen",
                    "scanned": summary.scanned,
                    "inserted": summary.inserted,
                    "conflict_skipped": summary.conflict_skipped,
                    "article_already_advanced": summary.article_already_advanced,
                })),
            )
            .await;

        summary
    }

    /// Phase 2：claim pending AI 任务，并发调用 AI、写 artifact、解析并 release。
    pub async fn process_ai_tasks(&self, opts: &AiRunOptions) -> AiProcessSummary {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "ai_run",
            repo: self.ctx.event_repo.as_ref(),
        };
        emitter
            .emit(
                "run_started",
                "info",
                None,
                None,
                "ai process run started",
                Some(json!({ "phase": "process" })),
            )
            .await;

        let owner = build_owner_id();
        let mut summary = AiProcessSummary::default();
        // F6-3: 0 = 不限。Option<u32> 表达"无上限"，命中上限主动 break + INFO。
        let cap: Option<u32> = if opts.max_batches == 0 {
            None
        } else {
            Some(opts.max_batches)
        };

        loop {
            if cap.is_some_and(|c| summary.batches_executed >= c) {
                summary.max_batches_reached = true;
                tracing::info!(
                    stage = "ai_run",
                    phase = "process",
                    batch_size = opts.process_batch_size,
                    max_batches = opts.max_batches,
                    batches_executed = summary.batches_executed,
                    "max_batches_per_run reached; remaining pending AI tasks will be picked up by next run"
                );
                break;
            }

            let now = OffsetDateTime::now_utc();
            let claimed = match self
                .ctx
                .ai_result_repo
                .claim_pending(&ClaimRequest {
                    owner: owner.clone(),
                    now,
                    lease_expires_at: lease_expires_at(
                        now,
                        Duration::seconds(self.ctx.app.lease.ai_duration_seconds as i64),
                    ),
                    batch_size: opts.process_batch_size.max(1),
                    max_attempts: opts.max_attempts,
                })
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    tracing::error!("failed to claim AI tasks: {error}");
                    emitter
                        .emit(
                            "run_completed",
                            "info",
                            None,
                            None,
                            "ai process run completed",
                            Some(json!({
                                "phase": "process",
                                "claimed": summary.claimed,
                                "claim_error": error.error_kind(),
                                "batches_executed": summary.batches_executed,
                            })),
                        )
                        .await;
                    return summary;
                }
            };

            if claimed.is_empty() {
                break;
            }

            summary.claimed += claimed.len() as u32;
            summary.batches_executed += 1;
            let per_task_len_before = summary.per_task.len();

            let semaphore = Arc::new(Semaphore::new(
                self.ctx.app.http.concurrent_fetches.max(1) as usize,
            ));
            let mut join_set = JoinSet::new();

            for task in claimed {
                let ctx = Arc::clone(&self.ctx);
                let owner = owner.clone();
                let opts = opts.clone();
                let semaphore = Arc::clone(&semaphore);
                join_set.spawn(async move {
                    let _permit = semaphore
                        .acquire_owned()
                        .await
                        .expect("semaphore should not be closed");
                    process_one(ctx, owner, task, opts).await
                });
            }

            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(outcome) => summary.per_task.push(outcome),
                    Err(error) => {
                        tracing::error!("AI task panicked or was cancelled: {error}");
                        summary.permanent_failed += 1;
                    }
                }
            }

            // F6-3: 同 ExtractFlow 路径，本批次产生 retryable 失败 ⇒ 这些
            // 任务已回到 pending 且 lease 释放，下次 claim 会立即捞回形成
            // retry-loop。主动终止，留待下一次 run 重试。
            let batch_retryable = summary.per_task[per_task_len_before..]
                .iter()
                .filter(|o| matches!(o.status, AiTaskStatus::RetryableFailed))
                .count();
            if batch_retryable > 0 {
                summary.retryable_deferred = true;
                tracing::info!(
                    stage = "ai_run",
                    phase = "process",
                    batches_executed = summary.batches_executed,
                    batch_retryable,
                    "retryable failures in batch; deferring re-claim to next run"
                );
                break;
            }
        }

        recalculate_process_summary(&mut summary);
        emitter
            .emit(
                "run_completed",
                "info",
                None,
                None,
                "ai process run completed",
                Some(json!({
                    "phase": "process",
                    "claimed": summary.claimed,
                    "succeeded": summary.succeeded,
                    "filtered": summary.filtered,
                    "retryable_failed": summary.retryable_failed,
                    "permanent_failed": summary.permanent_failed,
                    "batches_executed": summary.batches_executed,
                    "max_batches_reached": summary.max_batches_reached,
                    "retryable_deferred": summary.retryable_deferred,
                })),
            )
            .await;

        summary
    }

    /// 便利方法：先 task_gen，再 process。
    pub async fn run(&self, opts: AiRunOptions) -> AiRunSummary {
        let task_gen = self.task_gen(&opts).await;
        let process = self.process_ai_tasks(&opts).await;
        AiRunSummary { task_gen, process }
    }
}

async fn process_one(
    ctx: Arc<RunContext>,
    owner: String,
    claimed: ClaimedAiResult,
    opts: AiRunOptions,
) -> AiTaskOutcome {
    let emitter = RunEventEmitter {
        run_id: &ctx.run_id,
        stage: "ai_run",
        repo: ctx.event_repo.as_ref(),
    };
    let article = match ctx.article_repo.find_by_id(claimed.article_id).await {
        Ok(Some(article)) => article,
        Ok(None) => {
            let message = format!("article {} not found", claimed.article_id);
            return release_permanent_ai_failure(
                &ctx,
                &emitter,
                &owner,
                &claimed,
                &message,
                "article_missing",
                OffsetDateTime::now_utc(),
            )
            .await;
        }
        Err(error) => {
            let message = error.display_user();
            return release_permanent_ai_failure(
                &ctx,
                &emitter,
                &owner,
                &claimed,
                &message,
                error.error_kind(),
                OffsetDateTime::now_utc(),
            )
            .await;
        }
    };

    let task = AiTask {
        article_ai_result_id: claimed.id,
        article_id: claimed.article_id,
        title: article.title,
        body_text: truncate_chars(&article.body_text, opts.max_input_chars as usize),
        category_key: opts.category_key.clone(),
        prompt_template: opts.prompt_template.clone(),
        model_id: opts.model_id.clone(),
        max_tokens: opts.max_tokens,
        temperature: opts.temperature,
    };

    let response = match ctx.ai_client.invoke(&task).await {
        Ok(response) => response,
        Err(error) if error.is_retryable() => {
            return release_retryable_ai_failure(
                &ctx,
                &emitter,
                &owner,
                &claimed,
                error,
                OffsetDateTime::now_utc(),
            )
            .await;
        }
        Err(error) => {
            let message = error.display_user();
            return release_permanent_ai_failure(
                &ctx,
                &emitter,
                &owner,
                &claimed,
                &message,
                error.error_kind(),
                OffsetDateTime::now_utc(),
            )
            .await;
        }
    };

    let raw_artifact_id = write_ai_raw_response_artifact(&ctx, claimed.id, &response).await;
    let parsed = match parse_response(claimed.id, &response.raw_response) {
        Ok(parsed) => parsed,
        Err(error) => {
            let message = error.display_user();
            return release_permanent_ai_failure(
                &ctx,
                &emitter,
                &owner,
                &claimed,
                &message,
                error.error_kind(),
                OffsetDateTime::now_utc(),
            )
            .await;
        }
    };

    let keep = matches!(parsed, ParsedResponse::Output(_));
    let outcome = match success_outcome_from_response(parsed, raw_artifact_id, &response) {
        Ok(outcome) => outcome,
        Err(error) => {
            let message = error.display_user();
            return release_permanent_ai_failure(
                &ctx,
                &emitter,
                &owner,
                &claimed,
                &message,
                error.error_kind(),
                OffsetDateTime::now_utc(),
            )
            .await;
        }
    };

    let release = ctx
        .ai_result_repo
        .release_success_and_advance_article(
            claimed.id,
            &owner,
            outcome,
            claimed.article_id,
            i32::from(opts.min_importance_score.get()),
            OffsetDateTime::now_utc(),
        )
        .await;

    match release {
        Ok(ReleaseSuccessOutcome {
            released: true,
            article_advance,
        }) => {
            let status = if keep {
                AiTaskStatus::Succeeded
            } else {
                AiTaskStatus::Filtered
            };
            emitter
                .emit(
                    "ai_completed",
                    "info",
                    Some("article_ai_result"),
                    Some(claimed.id),
                    "AI task completed",
                    Some(json!({
                        "article_id": claimed.article_id,
                        "article_ai_result_id": claimed.id,
                        "status": ai_task_status_str(&status),
                        "article_advance": format!("{article_advance:?}"),
                        "raw_response_artifact_id": raw_artifact_id,
                    })),
                )
                .await;
            AiTaskOutcome {
                article_ai_result_id: claimed.id,
                article_id: claimed.article_id,
                status,
                article_advance: Some(article_advance),
                error_kind: None,
            }
        }
        Ok(ReleaseSuccessOutcome {
            released: false, ..
        }) => {
            tracing::warn!(ai_result_id = claimed.id, "AI success release conflicted");
            AiTaskOutcome {
                article_ai_result_id: claimed.id,
                article_id: claimed.article_id,
                status: AiTaskStatus::PermanentFailed,
                article_advance: Some(AiCompleteArticleAdvance::NoChange),
                error_kind: Some("lease_conflict".to_string()),
            }
        }
        Err(error) => {
            tracing::error!(
                ai_result_id = claimed.id,
                "AI success release failed: {error}"
            );
            AiTaskOutcome {
                article_ai_result_id: claimed.id,
                article_id: claimed.article_id,
                status: AiTaskStatus::PermanentFailed,
                article_advance: Some(AiCompleteArticleAdvance::NoChange),
                error_kind: Some(error.error_kind().to_string()),
            }
        }
    }
}

async fn write_ai_raw_response_artifact(
    ctx: &RunContext,
    ai_result_id: i64,
    response: &AiResponse,
) -> Option<i64> {
    let artifact_writer = ArtifactWriter {
        config: &ctx.app.artifact,
        repo: ctx.artifact_repo.as_ref(),
    };
    match artifact_writer
        .write_inline(
            "ai_raw_response",
            &ai_result_id.to_string(),
            response.raw_response.as_bytes(),
        )
        .await
    {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(
                ai_result_id,
                "failed to persist AI raw response artifact before release: {error}"
            );
            None
        }
    }
}

fn success_outcome_from_response(
    parsed: ParsedResponse,
    raw_artifact_id: Option<i64>,
    response: &AiResponse,
) -> Result<AiSuccessOutcome, AiError> {
    let usage = response.usage.as_ref();
    let (summary, tags_json, importance_score, keep_decision) = match parsed {
        ParsedResponse::Output(output) => {
            let tags_json = serde_json::to_string(&output.tags)
                .map_err(|err| AiError::InvalidJson(err.to_string()))?;
            (
                output.summary,
                tags_json,
                Some(i32::from(output.importance_score.get())),
                Some(output.keep_decision),
            )
        }
        ParsedResponse::Filtered(filtered) => {
            (filtered.reason, "[]".to_string(), None, Some(false))
        }
    };

    Ok(AiSuccessOutcome {
        summary,
        tags_json,
        importance_score,
        keep_decision,
        raw_response_artifact_id: raw_artifact_id,
        tokens_in: usage.map(|usage| clamp_u64_to_i32(usage.tokens_in)),
        tokens_out: usage.map(|usage| clamp_u64_to_i32(usage.tokens_out)),
        cost_micro_usd: usage.and_then(|usage| usage.cost_micro_usd),
        latency_ms: Some(clamp_u64_to_i32(response.latency_ms)),
    })
}

async fn release_retryable_ai_failure(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    claimed: &ClaimedAiResult,
    error: AiError,
    now: OffsetDateTime,
) -> AiTaskOutcome {
    let message = error.display_user();
    let kind = error.error_kind().to_string();
    match ctx
        .ai_result_repo
        .release_retryable_failure(claimed.id, owner, &message, &kind, now)
        .await
    {
        Ok(false) => tracing::warn!(ai_result_id = claimed.id, "AI retryable release conflicted"),
        Err(error) => tracing::error!(
            ai_result_id = claimed.id,
            "AI retryable release failed: {error}"
        ),
        Ok(true) => {}
    }
    emitter
        .emit(
            "ai_failed",
            "warn",
            Some("article_ai_result"),
            Some(claimed.id),
            &message,
            Some(json!({
                "article_id": claimed.article_id,
                "article_ai_result_id": claimed.id,
                "error_kind": kind,
                "retryable": true,
            })),
        )
        .await;
    AiTaskOutcome {
        article_ai_result_id: claimed.id,
        article_id: claimed.article_id,
        status: AiTaskStatus::RetryableFailed,
        article_advance: None,
        error_kind: Some(kind),
    }
}

async fn release_permanent_ai_failure(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    claimed: &ClaimedAiResult,
    message: &str,
    kind: &str,
    now: OffsetDateTime,
) -> AiTaskOutcome {
    match ctx
        .ai_result_repo
        .release_permanent_failure(claimed.id, owner, message, kind, now)
        .await
    {
        Ok(false) => tracing::warn!(ai_result_id = claimed.id, "AI permanent release conflicted"),
        Err(error) => tracing::error!(
            ai_result_id = claimed.id,
            "AI permanent release failed: {error}"
        ),
        Ok(true) => {}
    }
    emitter
        .emit(
            "ai_failed",
            "error",
            Some("article_ai_result"),
            Some(claimed.id),
            message,
            Some(json!({
                "article_id": claimed.article_id,
                "article_ai_result_id": claimed.id,
                "error_kind": kind,
                "retryable": false,
            })),
        )
        .await;
    AiTaskOutcome {
        article_ai_result_id: claimed.id,
        article_id: claimed.article_id,
        status: AiTaskStatus::PermanentFailed,
        article_advance: None,
        error_kind: Some(kind.to_string()),
    }
}

fn recalculate_process_summary(summary: &mut AiProcessSummary) {
    summary.succeeded = 0;
    summary.filtered = 0;
    summary.retryable_failed = 0;
    summary.permanent_failed = 0;

    for outcome in &summary.per_task {
        match outcome.status {
            AiTaskStatus::Succeeded => summary.succeeded += 1,
            AiTaskStatus::Filtered => summary.filtered += 1,
            AiTaskStatus::RetryableFailed => summary.retryable_failed += 1,
            AiTaskStatus::PermanentFailed => summary.permanent_failed += 1,
        }
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn clamp_u64_to_i32(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn ai_task_status_str(status: &AiTaskStatus) -> &'static str {
    match status {
        AiTaskStatus::Succeeded => "succeeded",
        AiTaskStatus::Filtered => "filtered",
        AiTaskStatus::RetryableFailed => "retryable_failed",
        AiTaskStatus::PermanentFailed => "permanent_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_run_options_min_importance_score_is_strong_typed() {
        // F6-1: AiRunOptions.min_importance_score 必须是 Score0To100，
        // 与 publish (PublishFreezeOptions) / config (PublishConfig) 三方对齐。
        // 该测试防止后续改回 i32 的回归 —— 类型 mismatch 会让编译失败。
        let opts = AiRunOptions {
            task_gen_batch_size: 1,
            process_batch_size: 1,
            max_attempts: 1,
            prompt_template: String::new(),
            model_id: String::new(),
            max_input_chars: 0,
            max_tokens: 0,
            temperature: 0.0,
            min_importance_score: Score0To100::try_new(50).unwrap(),
            max_batches: 0,
            category_key: "x".to_string(),
            prompt_version: 1,
            output_schema_version: 1,
        };
        // newtype 提供 type-safe 0..=100 不变量；该断言只是文档化运行时值。
        assert_eq!(opts.min_importance_score.get(), 50);
    }
}
