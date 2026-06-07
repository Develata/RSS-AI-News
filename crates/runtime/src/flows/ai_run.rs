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
    /// W14-A 失败回退链（已由 config effective 层 trim / 去重 / 去主模型）。process
    /// 阶段主模型锚定 `claimed.model_id`（行身份），链 = `[主模型, ...fallback_models]`。
    /// 空 = 不回退。task_gen 不使用此字段（只用主模型建 pending 行）。
    pub fallback_models: Vec<String>,
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
    /// CLI 调用前通过 `rule_version_repo.get_or_create("prompt", version_tag, ...)`
    /// 在 `rule_versions` 表中找到或插入对应 `(kind, version_tag)` 行后得到的
    /// `rule_versions.id`，写入 `article_ai_results.prompt_version`（详见
    /// storage-schema §4.6 幂等四元组）。注意这是按 tag 解析得到的 id，不是
    /// "active prompt"（本仓库无 active resolver 语义）。
    pub prompt_version: i64,
    /// CLI 调用前通过 `rule_version_repo.get_or_create("ai_output_schema",
    /// version_tag, ...)` 在 `rule_versions` 表中找到或插入对应 `(kind,
    /// version_tag)` 行后得到的 `rule_versions.id`，写入
    /// `article_ai_results.output_schema_version`。
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

    // W14-A：模型链 = [主模型(claimed.model_id 行身份), ...fallback]，去重。主模型锚定
    // claimed.model_id（幂等键）而非 opts.model_id —— pending 行可能由旧配置创建，重试须
    // 沿用其身份模型（codex P2 BLOCKER）。一次"模型尝试" = invoke + 写 artifact + parse +
    // outcome 整段；任一步失败且 should_fallback 即试下一个模型（内容类错误在 parse 阶段，
    // 故循环须包住整段而非仅 invoke，codex P3 BLOCKER）。
    let chain = build_model_chain(&claimed.model_id, &opts.fallback_models);
    let mut attempts: Vec<serde_json::Value> = Vec::with_capacity(chain.len());
    let mut last_error: Option<AiError> = None;

    for (attempt_index, model) in chain.iter().enumerate() {
        let task = AiTask {
            article_ai_result_id: claimed.id,
            article_id: claimed.article_id,
            title: article.title.clone(),
            body_text: truncate_chars(&article.body_text, opts.max_input_chars as usize),
            category_key: opts.category_key.clone(),
            prompt_template: opts.prompt_template.clone(),
            model_id: model.clone(),
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
        };

        match run_model_attempt(&ctx, &task, attempt_index).await {
            Ok(attempt) => {
                return finish_ai_success(
                    &ctx, &emitter, &owner, &claimed, &opts, attempt, model, &chain[0], &attempts,
                )
                .await;
            }
            Err(error) => {
                attempts.push(json!({
                    "attempted_model_id": model,
                    "error_kind": error.error_kind(),
                    "should_fallback": error.should_fallback(),
                }));
                let try_next = error.should_fallback() && attempt_index + 1 < chain.len();
                last_error = Some(error);
                if !try_next {
                    break;
                }
            }
        }
    }

    let error = last_error.expect("model chain is non-empty, so at least one attempt ran");
    finish_ai_failure(&ctx, &emitter, &owner, &claimed, error, &attempts).await
}

/// W14-A：构造模型尝试链 `[主模型, ...fallback]`，主模型恒在首位，其余 trim / 去空白 /
/// 去主模型 / 链内去重。effective 层已规范化 fallback，但主模型此处取 `claimed.model_id`
/// （行身份，可能与当前配置主模型不同），故仍在此去重一次。
fn build_model_chain(primary: &str, fallback_models: &[String]) -> Vec<String> {
    let mut chain = vec![primary.to_string()];
    for model in fallback_models {
        let model = model.trim();
        if !model.is_empty() && !chain.iter().any(|existing| existing == model) {
            chain.push(model.to_string());
        }
    }
    chain
}

/// 一次模型尝试的成功产物。
struct SuccessfulAttempt {
    outcome: AiSuccessOutcome,
    keep: bool,
    raw_artifact_id: Option<i64>,
}

/// 单个模型的一次完整尝试：invoke + 写 raw artifact + parse + 构造 outcome。
/// 任一步失败返回 `AiError`，由 [`process_one`] 按 `should_fallback` 决定换模型 / 终止。
async fn run_model_attempt(
    ctx: &RunContext,
    task: &AiTask,
    attempt_index: usize,
) -> Result<SuccessfulAttempt, AiError> {
    let response = ctx.ai_client.invoke(task).await?;
    let raw_artifact_id =
        write_ai_raw_response_artifact(ctx, task.article_ai_result_id, attempt_index, &response)
            .await;
    let parsed = parse_response(task.article_ai_result_id, &response.raw_response)?;
    let keep = matches!(parsed, ParsedResponse::Output(_));
    let outcome = success_outcome_from_response(parsed, raw_artifact_id, &response)?;
    Ok(SuccessfulAttempt {
        outcome,
        keep,
        raw_artifact_id,
    })
}

/// 成功路径：写回结果（`effective_model_id = actual_model`，不碰幂等键 `model_id`），
/// 降级时额外 emit `ai_model_fallback`，并发 `ai_completed`。
#[allow(clippy::too_many_arguments)]
async fn finish_ai_success(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    claimed: &ClaimedAiResult,
    opts: &AiRunOptions,
    attempt: SuccessfulAttempt,
    actual_model: &str,
    primary_model: &str,
    attempts: &[serde_json::Value],
) -> AiTaskOutcome {
    let SuccessfulAttempt {
        outcome,
        keep,
        raw_artifact_id,
    } = attempt;

    if actual_model != primary_model {
        emitter
            .emit(
                "ai_model_fallback",
                "warn",
                Some("article_ai_result"),
                Some(claimed.id),
                "AI fell back to a non-primary model",
                Some(json!({
                    "article_id": claimed.article_id,
                    "article_ai_result_id": claimed.id,
                    "primary_model_id": primary_model,
                    "actual_model_id": actual_model,
                    "attempts": attempts,
                })),
            )
            .await;
    }

    let release = ctx
        .ai_result_repo
        .release_success_and_advance_article(
            claimed.id,
            owner,
            outcome,
            actual_model,
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
                        "primary_model_id": primary_model,
                        "actual_model_id": actual_model,
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

/// 失败路径（模型链耗尽或遇到不可回退错误）：发生过多模型尝试时先 emit 完整尝试链
/// （codex P3 MEDIUM），再按最后错误的 `is_retryable()` 回队 / 永久失败。
async fn finish_ai_failure(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    claimed: &ClaimedAiResult,
    error: AiError,
    attempts: &[serde_json::Value],
) -> AiTaskOutcome {
    let now = OffsetDateTime::now_utc();
    if attempts.len() > 1 {
        emitter
            .emit(
                "ai_fallback_exhausted",
                "warn",
                Some("article_ai_result"),
                Some(claimed.id),
                "AI fallback chain exhausted",
                Some(json!({
                    "article_id": claimed.article_id,
                    "article_ai_result_id": claimed.id,
                    "attempts": attempts,
                    "final_error_kind": error.error_kind(),
                })),
            )
            .await;
    }

    if error.is_retryable() {
        release_retryable_ai_failure(ctx, emitter, owner, claimed, error, now).await
    } else {
        let message = error.display_user();
        let kind = error.error_kind().to_string();
        release_permanent_ai_failure(ctx, emitter, owner, claimed, &message, &kind, now).await
    }
}

async fn write_ai_raw_response_artifact(
    ctx: &RunContext,
    ai_result_id: i64,
    attempt_index: usize,
    response: &AiResponse,
) -> Option<i64> {
    // W14-A：attempt 0 用 `ai_result_id` 作 key（无 fallback 时与历史一致、replay 兼容）；
    // fallback 尝试用 `{id}#a{idx}` 各自留一份，避免 upsert 互相覆盖（codex P3 MEDIUM）。
    // 获胜尝试的 artifact id 经 article_ai_results.raw_response_artifact_id 始终可达。
    let artifact_key = if attempt_index == 0 {
        ai_result_id.to_string()
    } else {
        format!("{ai_result_id}#a{attempt_index}")
    };
    let artifact_writer = ArtifactWriter {
        config: &ctx.app.artifact,
        repo: ctx.artifact_repo.as_ref(),
    };
    match artifact_writer
        .write_inline(
            "ai_raw_response",
            &artifact_key,
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
        tokens_in: usage.map(|usage| clamp_u64_to_i64(usage.tokens_in)),
        tokens_out: usage.map(|usage| clamp_u64_to_i64(usage.tokens_out)),
        cost_micro_usd: usage.and_then(|usage| usage.cost_micro_usd),
        latency_ms: Some(clamp_u64_to_i64(response.latency_ms)),
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

fn clamp_u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
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
    fn model_chain_puts_primary_first_and_dedups() {
        // 主模型恒首位；trim / 去空白 / 去主模型 / 链内去重。
        let chain = build_model_chain(
            "gpt-primary",
            &[
                "  gpt-primary  ".to_string(), // == 主模型 → 去除
                "deepseek".to_string(),
                " deepseek ".to_string(), // 链内重复 → 去除
                String::new(),            // 空白 → 去除
                "claude".to_string(),
            ],
        );
        assert_eq!(chain, ["gpt-primary", "deepseek", "claude"]);
    }

    #[test]
    fn model_chain_without_fallback_is_just_primary() {
        assert_eq!(build_model_chain("only", &[]), ["only"]);
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
