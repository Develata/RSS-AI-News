//! ai_run flow：task_gen（建 pending AI 任务）+ process（claim → 调 AI → 回写）。
//!
//! 单条任务的执行机制见 `process`，结果回写见 `release`，DTO 见 `dto`。

use std::sync::Arc;

use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_storage::{
    ClaimRequest, ClaimedAiResult, FreshWindow, NewAiResult, ShareMark, ShareSide, ShareStage,
    build_owner_id, lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::task::JoinSet;

use crate::context::AiDeps;
use crate::events::RunEventEmitter;
use crate::flows::maintenance::{emit_maintenance_outcome, purge_expired_artifacts};

mod dto;
mod process;
mod release;

pub use dto::*;

pub struct AiRunFlow {
    ctx: Arc<AiDeps>,
}

impl AiRunFlow {
    pub fn new(ctx: Arc<AiDeps>) -> Self {
        Self { ctx }
    }

    /// Phase 1：为 `articles.state='persisted'` 的文章创建 pending AI 任务。
    ///
    /// 一次 pass 推进至多 `task_gen_batch_size` 篇 article（新插入 + 冲突对账，
    /// 记 `article_advanced`），由两侧分享（C9，plan/03 §5.1）：
    /// - fresh：publish 窗口内（若此刻 freeze 即可入选）的文章——尚无本身份任务
    ///   行的新插入与已有任务行的 C3 对账共用一个列表与 fresh 名额（R3E-B1），
    ///   共用一个顺序：来源时刻最早者优先（最先掉出窗口），同时刻按 article id
    ///   （R4-3）；keyset 游标越过推进不了的行，只有实际推进才计名额；
    /// - backlog：窗口外的 `persisted`，按 id 升序 + keyset 游标（C3：越过冲突后
    ///   仍留在 `persisted` 的行，并对账已有任务行）。
    ///
    /// 先行一侧得 `ceil(n/2)`，另一侧得余下，某侧不足时名额转给另一侧（两侧都扫完
    /// 才少于 n）。n ≥ 2 时 fresh 先行；n = 1 时先行侧是该分类上一次 task_gen
    /// **实际推进**的文章所属侧的另一侧（`ai_fresh_share_state`，与推进同事务写入，
    /// R2-B1）。某页出现插入存储错误时本 pass 停止。
    pub async fn task_gen(&self, opts: &AiRunOptions) -> TaskGenSummary {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run.run_id,
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

        let page_size = opts.task_gen_batch_size.max(1);
        // One window for the whole pass, ending at its own `now` (R2-S1).
        let now = (self.ctx.clock)();
        let window = FreshWindow {
            since: ai_fresh_since(now, opts.candidate_window_hours),
            until: now,
        };
        let mut summary = TaskGenSummary::default();
        let mut pass = TaskGenPass {
            page_size,
            window,
            backlog_cursor: 0,
            fresh_cursor: 0,
            stopped: false,
        };
        let fresh_first = if page_size == 1 {
            match self
                .ctx
                .ai_result_repo
                .last_served_side(&opts.category_key, ShareStage::TaskGen)
                .await
            {
                Ok(last) => last != Some(ShareSide::Fresh),
                Err(error) => {
                    tracing::error!("failed to read the task_gen alternation state: {error}");
                    summary.list_error = Some(error.error_kind().to_string());
                    pass.stopped = true;
                    true
                }
            }
        } else {
            true
        };
        let order = if fresh_first {
            [TaskGenSide::Fresh, TaskGenSide::Backlog]
        } else {
            [TaskGenSide::Backlog, TaskGenSide::Fresh]
        };
        let first_quota = page_size.div_ceil(2);
        let first = self
            .task_gen_side(order[0], first_quota, opts, &mut pass, &mut summary)
            .await;
        let second = self
            .task_gen_side(
                order[1],
                page_size - first.handled,
                opts,
                &mut pass,
                &mut summary,
            )
            .await;
        let left = page_size - first.handled - second.handled;
        if left > 0 && !first.exhausted {
            self.task_gen_side(order[0], left, opts, &mut pass, &mut summary)
                .await;
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
                    "fresh_advanced": summary.fresh_advanced,
                    "fresh_first": fresh_first,
                    "conflict_skipped": summary.conflict_skipped,
                    "reconciled": summary.reconciled,
                    "article_already_advanced": summary.article_already_advanced,
                    "error_kind": summary.list_error,
                })),
            )
            .await;

        summary
    }

    /// Drains one side's list until `quota` articles advanced, the list is
    /// exhausted, or a storage error stops the pass. Each list has a keyset
    /// cursor (the last scanned row), so a row that does not advance is
    /// passed without using the quota and is not re-read in this pass; a
    /// re-entry for leftover quota continues after it.
    async fn task_gen_side(
        &self,
        side: TaskGenSide,
        quota: u32,
        opts: &AiRunOptions,
        pass: &mut TaskGenPass,
        summary: &mut TaskGenSummary,
    ) -> SideOutcome {
        let mut outcome = SideOutcome::default();
        if quota == 0 || pass.stopped {
            return outcome;
        }
        let identity = NewAiResult {
            article_id: 0,
            prompt_version: opts.prompt_version,
            output_schema_version: opts.output_schema_version,
            model_id: opts.model_id.clone(),
        };
        loop {
            let listed = match side {
                TaskGenSide::Fresh => {
                    self.ctx
                        .article_repo
                        .list_fresh_for_ai_task_gen(
                            &opts.category_key,
                            pass.window,
                            pass.page_size,
                            pass.fresh_cursor,
                        )
                        .await
                }
                TaskGenSide::Backlog => {
                    self.ctx
                        .article_repo
                        .list_backlog_for_ai_task_gen(
                            &opts.category_key,
                            &identity,
                            pass.window,
                            pass.page_size,
                            pass.backlog_cursor,
                        )
                        .await
                }
            };
            let candidates = match listed {
                Ok(candidates) => candidates,
                Err(error) => {
                    tracing::error!("failed to list AI task candidates: {error}");
                    summary.list_error = Some(error.error_kind().to_string());
                    pass.stopped = true;
                    return outcome;
                }
            };
            let page_len = candidates.len();
            let failed_before = summary.insert_failed;
            for candidate in candidates {
                match side {
                    TaskGenSide::Backlog => pass.backlog_cursor = candidate.article_id,
                    TaskGenSide::Fresh => pass.fresh_cursor = candidate.article_id,
                }
                summary.scanned += 1;
                let item = NewAiResult {
                    article_id: candidate.article_id,
                    ..identity.clone()
                };
                let min_score = i32::from(opts.min_importance_score.get());
                let now = (self.ctx.clock)();
                // Batch size 1 records which side the advanced article belongs
                // to (its own window membership), atomically with the advance
                // (R2-B1).
                let inserted = if pass.page_size == 1 {
                    let mark = ShareMark {
                        category_key: &opts.category_key,
                        stage: ShareStage::TaskGen,
                        side: if candidate.in_window {
                            ShareSide::Fresh
                        } else {
                            ShareSide::Backlog
                        },
                    };
                    self.ctx
                        .ai_result_repo
                        .insert_pending_advance_and_mark(&item, min_score, now, &mark)
                        .await
                } else {
                    self.ctx
                        .ai_result_repo
                        .insert_pending_and_advance_article(&item, min_score, now)
                        .await
                };
                match inserted {
                    Ok(insert) => {
                        if insert.ai_result_id.is_some() {
                            summary.inserted += 1;
                        } else {
                            summary.conflict_skipped += 1;
                        }
                        if insert.reconciled {
                            summary.reconciled += 1;
                        }
                        if insert.article_already_advanced {
                            summary.article_already_advanced += 1;
                        }
                        if insert.article_advanced {
                            outcome.handled += 1;
                            if candidate.in_window {
                                summary.fresh_advanced += 1;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            article_id = candidate.article_id,
                            "failed to insert AI pending result: {error}"
                        );
                        summary.insert_failed += 1;
                    }
                }
                if outcome.handled >= quota {
                    break;
                }
            }
            // R2-S4: an insert error on this page stops the pass on every
            // exit path (quota met, short page, next page), so no side scans
            // on after it.
            if summary.insert_failed > failed_before {
                pass.stopped = true;
                return outcome;
            }
            if outcome.handled >= quota {
                return outcome;
            }
            if page_len < pass.page_size as usize {
                outcome.exhausted = true;
                return outcome;
            }
        }
    }

    /// Claims one batch, shared between in-window ("fresh") pending tasks and
    /// the rest (C9, plan/03 §5.2), as one storage transaction (R2-B2): a
    /// failing side rolls the whole batch back instead of stranding claimed
    /// rows the flow never receives.
    async fn claim_batch(
        &self,
        opts: &AiRunOptions,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedAiResult>, rss_ai_news_storage::StorageError> {
        let since = ai_fresh_since(request.now, opts.candidate_window_hours);
        self.ctx
            .ai_result_repo
            .claim_pending_shared(request, &opts.category_key, since)
            .await
    }

    /// Phase 2：claim pending AI 任务，并发调用 AI、写 artifact、解析并 release。
    pub async fn process_ai_tasks(&self, opts: &AiRunOptions) -> AiProcessSummary {
        let opts = Arc::new(opts.clone());
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run.run_id,
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

        purge_expired_artifacts(self.ctx.artifact_repo.as_ref(), &emitter).await;
        // W15 §5：首次 claim 前执行一次 ① reclaim + ② sweep（顺序固定，best-effort）。
        let maintenance_now = (self.ctx.clock)();
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

            let now = (self.ctx.clock)();
            let claimed = match self
                .claim_batch(
                    &opts,
                    &ClaimRequest {
                        owner: owner.clone(),
                        now,
                        lease_expires_at: lease_expires_at(
                            now,
                            Duration::seconds(self.ctx.lease.ai_duration_seconds as i64),
                        ),
                        batch_size: opts.process_batch_size.max(1),
                        max_attempts: opts.max_attempts,
                    },
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
                    summary.claim_error = Some(error.error_kind().to_string());
                    return summary;
                }
            };

            if claimed.is_empty() {
                break;
            }

            summary.claimed += claimed.len() as u32;
            summary.batches_executed += 1;
            let retryable_before = summary.retryable_failed;

            let concurrency = self.ctx.http.concurrent_fetches.max(1) as usize;
            let mut join_set = JoinSet::new();
            let mut pending = claimed.into_iter();
            loop {
                // Only materialize task state for the next concurrency window.
                while join_set.len() < concurrency {
                    let Some(task) = pending.next() else { break };
                    let ctx = Arc::clone(&self.ctx);
                    let owner = owner.clone();
                    let opts = Arc::clone(&opts);
                    join_set.spawn(process::process_one(ctx, owner, task, opts));
                }
                let Some(result) = join_set.join_next().await else {
                    break;
                };
                match result {
                    Ok(outcome) => summary.record(outcome),
                    Err(error) => {
                        tracing::error!("AI task panicked or was cancelled: {error}");
                        summary.tasks_panicked += 1;
                    }
                }
            }

            // F6-3: 同 ExtractFlow 路径，本批次产生 retryable 失败 ⇒ 这些
            // 任务已回到 pending 且 lease 释放，下次 claim 会立即捞回形成
            // retry-loop。主动终止，留待下一次 run 重试。
            let batch_retryable = summary.retryable_failed - retryable_before;
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
                    "lease_lost": summary.lease_lost,
                    "release_failed": summary.release_failed,
                    "tasks_panicked": summary.tasks_panicked,
                    "storage_failed": summary.storage_failed,
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

/// The task_gen list a side drains (C9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskGenSide {
    /// In-window, inserts and C3 reconciliations in one order: source
    /// instant, then article id (R4-3); keyset cursor over that order.
    Fresh,
    /// Outside the window, id order + cursor.
    Backlog,
}

/// State shared by the sides of one task_gen pass.
struct TaskGenPass {
    page_size: u32,
    window: FreshWindow,
    /// C3 keyset cursor of the backlog (id-ordered) list.
    backlog_cursor: i64,
    /// Keyset cursor of the fresh (source-time ordered) list: the article id
    /// of the last scanned row, whose source instant the query resolves.
    fresh_cursor: i64,
    /// A storage error ended the pass.
    stopped: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct SideOutcome {
    handled: u32,
    exhausted: bool,
}

/// Start of the publish window, from the freeze's own rule
/// ([`crate::flows::publish::published_since`]): an article counts as fresh
/// when it would be eligible if publish froze now.
fn ai_fresh_since(now: OffsetDateTime, candidate_window_hours: u32) -> OffsetDateTime {
    crate::flows::publish::published_since(now, candidate_window_hours)
}

impl AiProcessSummary {
    /// Count every outcome; retain only the first 32 failures for diagnostics.
    fn record(&mut self, outcome: AiTaskOutcome) {
        if outcome.storage_failed {
            self.storage_failed += 1;
        }
        match outcome.status {
            AiTaskStatus::Succeeded => self.succeeded += 1,
            AiTaskStatus::Filtered => self.filtered += 1,
            AiTaskStatus::RetryableFailed => self.retryable_failed += 1,
            AiTaskStatus::PermanentFailed => self.permanent_failed += 1,
            AiTaskStatus::LeaseLost => self.lease_lost += 1,
            AiTaskStatus::ReleaseFailed => self.release_failed += 1,
        }
        if !matches!(
            outcome.status,
            AiTaskStatus::Succeeded | AiTaskStatus::Filtered
        ) && self.failure_samples.len() < 32
        {
            self.failure_samples.push(outcome);
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
            prompt_template: String::new().into(),
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
            candidate_window_hours: 48,
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

    #[test]
    fn summary_bounds_samples_without_losing_counts() {
        let mut summary = AiProcessSummary {
            tasks_panicked: 4,
            ..Default::default()
        };
        for _ in 0..10_000 {
            summary.record(AiTaskOutcome {
                storage_failed: false,
                article_ai_result_id: 1,
                article_id: 1,
                article_advance: None,
                status: AiTaskStatus::Succeeded,
                error_kind: None,
            });
            summary.record(AiTaskOutcome {
                storage_failed: false,
                article_ai_result_id: 1,
                article_id: 1,
                article_advance: None,
                status: AiTaskStatus::PermanentFailed,
                error_kind: Some("failure".into()),
            });
        }
        summary.record(AiTaskOutcome {
            storage_failed: false,
            article_ai_result_id: 1,
            article_id: 1,
            article_advance: None,
            status: AiTaskStatus::RetryableFailed,
            error_kind: None,
        });
        assert_eq!(summary.succeeded, 10_000);
        assert_eq!(summary.permanent_failed, 10_000);
        assert_eq!(summary.retryable_failed, 1);
        assert_eq!(summary.failure_samples.len(), 32);
        assert_eq!(summary.tasks_panicked, 4);
    }
}
