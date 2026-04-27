use std::sync::Arc;

use rss_ai_news_ai::{AiError, AiResponse, AiTask, ParsedResponse, parse_response};
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
    pub min_importance_score: i32,
    pub category_key: String,
    /// stub：本轮固定 1。
    pub prompt_version: i64,
    /// stub：本轮固定 1。
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

        let now = OffsetDateTime::now_utc();
        let owner = build_owner_id();
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
                            "claimed": 0,
                            "claim_error": error.error_kind(),
                        })),
                    )
                    .await;
                return AiProcessSummary::default();
            }
        };

        let mut summary = AiProcessSummary {
            claimed: claimed.len() as u32,
            ..AiProcessSummary::default()
        };
        let semaphore = Arc::new(Semaphore::new(
            self.ctx.app.http.concurrent_fetches.max(1) as usize
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
            opts.min_importance_score,
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
