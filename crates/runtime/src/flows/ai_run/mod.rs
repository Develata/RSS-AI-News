//! ai_run flow：task_gen（建 pending AI 任务）+ process（claim → 调 AI → 回写）。
//!
//! 单条任务的执行机制见 [`process`]，结果回写见 [`release`]，DTO 见 [`dto`]。

use std::sync::Arc;

use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_storage::{ClaimRequest, NewAiResult, build_owner_id, lease_expires_at};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::context::RunContext;
use crate::events::RunEventEmitter;
use crate::flows::maintenance::emit_maintenance_outcome;

mod dto;
mod process;
mod release;

pub use dto::*;

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
            .list_persisted_for_ai_task_gen(&opts.category_key, opts.task_gen_batch_size.max(1), 0)
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

        // W15 §5：首次 claim 前执行一次 ① reclaim + ② sweep（顺序固定，best-effort）。
        let maintenance_now = OffsetDateTime::now_utc();
        let reclaimed = self
            .ctx
            .ai_result_repo
            .reclaim_expired_leases(maintenance_now)
            .await;
        let swept = self
            .ctx
            .ai_result_repo
            .terminalize_exhausted(opts.max_attempts, maintenance_now)
            .await;
        emit_maintenance_outcome(&emitter, "article_ai_results", reclaimed, Some(swept)).await;

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
                .claim_pending(
                    &ClaimRequest {
                        owner: owner.clone(),
                        now,
                        lease_expires_at: lease_expires_at(
                            now,
                            Duration::seconds(self.ctx.app.lease.ai_duration_seconds as i64),
                        ),
                        batch_size: opts.process_batch_size.max(1),
                        max_attempts: opts.max_attempts,
                    },
                    &opts.category_key,
                )
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
                    process::process_one(ctx, owner, task, opts).await
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

/// W14-A 运行前 lease 预算（秒）：fallback 把一次 attempt 变成最多 `1+fallback_len` 次 HTTP；
/// process 又是整批先 claim（一次拿一批 lease）再按 `concurrent_fetches` 限流执行，后排任务
/// 排队也耗 lease。最坏耗时 ≈ `ceil(batch/concurrent) × request_timeout × (1+fallback_len)`，
/// 调用方须校验其 ≤ `lease.ai_duration_seconds`（codex P3 BLOCKER：含批排队；batch 是运行时
/// CLI 参数，故在组装实际值后运行前校验，而非配置静态校验）。
pub fn ai_lease_budget_seconds(
    process_batch_size: u32,
    concurrent_fetches: u32,
    request_timeout_seconds: u64,
    fallback_len: usize,
) -> u64 {
    let concurrent = concurrent_fetches.max(1);
    let waves = u64::from(process_batch_size.max(1).div_ceil(concurrent));
    waves
        .saturating_mul(request_timeout_seconds)
        .saturating_mul(1 + fallback_len as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rss_ai_news_domain::Score0To100;

    #[test]
    fn ai_run_options_min_importance_score_is_strong_typed() {
        // F6-1: AiRunOptions.min_importance_score 必须是 Score0To100,
        // 与 publish (PublishFreezeOptions) / config (PublishConfig) 三方对齐。
        // 该测试防止后续改回 i32 的回归 —— 类型 mismatch 会让编译失败。
        let opts = AiRunOptions {
            task_gen_batch_size: 1,
            process_batch_size: 1,
            max_attempts: 1,
            prompt_template: String::new(),
            model_id: String::new(),
            fallback_models: Vec::new(),
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

    #[test]
    fn lease_budget_accounts_for_batch_queueing_and_chain_len() {
        // batch=10, concurrent=5 → 2 波；request_timeout=60；fallback_len=2 → ×3。
        // 2 × 60 × 3 = 360。
        assert_eq!(ai_lease_budget_seconds(10, 5, 60, 2), 360);
        // 无 fallback：2 × 60 × 1 = 120。
        assert_eq!(ai_lease_budget_seconds(10, 5, 60, 0), 120);
        // batch ≤ concurrent → 1 波。
        assert_eq!(ai_lease_budget_seconds(3, 5, 60, 1), 120);
        // concurrent=0 兜底为 1 波/项（不 panic）。
        assert_eq!(ai_lease_budget_seconds(4, 0, 10, 0), 40);
    }
}
