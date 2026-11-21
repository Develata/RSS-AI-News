//! AI 任务结果回写：成功 / 失败（可重试 / 永久）三条 release 路径 + 事件发射。
//! 尝试机制（invoke / parse / 构造 outcome）在 [`super::process`]。

use rss_ai_news_ai::AiError;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_storage::{ClaimedAiResult, ReleaseSuccessOutcome};
use serde_json::json;
use time::OffsetDateTime;

use crate::context::AiDeps;
use crate::events::RunEventEmitter;

use super::process::SuccessfulAttempt;
use super::{AiRunOptions, AiTaskOutcome, AiTaskStatus};

/// 成功路径：写回结果（`effective_model_id = actual_model`，不碰幂等键 `model_id`），
/// 降级时额外 emit `ai_model_fallback`，并发 `ai_completed`。
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_ai_success(
    ctx: &AiDeps,
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
            (ctx.clock)(),
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
                storage_failed: false,
                article_ai_result_id: claimed.id,
                article_id: claimed.article_id,
                status,
                article_advance: Some(article_advance),
                error_kind: None,
            }
        }
        Ok(ReleaseSuccessOutcome {
            released: false, ..
        }) => release_not_applied(emitter, claimed, "success", NotApplied::LeaseLost).await,
        Err(error) => {
            release_not_applied(emitter, claimed, "success", NotApplied::Storage(&error)).await
        }
    }
}

/// Why a release changed nothing.
enum NotApplied<'a> {
    /// Owner guard matched no row: another owner holds the lease now.
    LeaseLost,
    /// The release statement failed; the row keeps this run's lease until it
    /// expires and is reclaimed.
    Storage(&'a rss_ai_news_storage::StorageError),
}

/// Outcome of a release that did not land: no `ai_completed` / `ai_failed`
/// event and no state counted, only `ai_release_not_applied` with the reason.
async fn release_not_applied(
    emitter: &RunEventEmitter<'_>,
    claimed: &ClaimedAiResult,
    attempted: &str,
    reason: NotApplied<'_>,
) -> AiTaskOutcome {
    let (status, severity, error_kind) = match reason {
        NotApplied::LeaseLost => {
            tracing::warn!(
                ai_result_id = claimed.id,
                attempted,
                "AI release matched no row (lease lost)"
            );
            (AiTaskStatus::LeaseLost, "warn", "lease_lost".to_string())
        }
        NotApplied::Storage(error) => {
            tracing::error!(
                ai_result_id = claimed.id,
                attempted,
                "AI release failed: {error}"
            );
            (
                AiTaskStatus::ReleaseFailed,
                "error",
                error.error_kind().to_string(),
            )
        }
    };
    emitter
        .emit(
            "ai_release_not_applied",
            severity,
            Some("article_ai_result"),
            Some(claimed.id),
            "AI result release did not apply",
            Some(json!({
                "article_id": claimed.article_id,
                "article_ai_result_id": claimed.id,
                "attempted": attempted,
                "error_kind": error_kind,
            })),
        )
        .await;
    AiTaskOutcome {
        storage_failed: false,
        article_ai_result_id: claimed.id,
        article_id: claimed.article_id,
        status,
        article_advance: None,
        error_kind: Some(error_kind),
    }
}

/// 失败路径（模型链耗尽或遇到不可回退错误）：发生过多模型尝试时先 emit 完整尝试链
/// （codex P3 MEDIUM），再按最后错误的 `is_retryable()` 回队 / 永久失败。
/// W15 §3：retryable 路径在 release SQL 内按 `max_attempts` 折叠——预算耗尽
/// 直接转 `permanent_failed`，不再造出永久卡 pending 的行。
pub(super) async fn finish_ai_failure(
    ctx: &AiDeps,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    claimed: &ClaimedAiResult,
    error: AiError,
    max_attempts: u32,
    attempts: &[serde_json::Value],
) -> AiTaskOutcome {
    let now = (ctx.clock)();
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

    let message = error.display_user();
    let kind = error.error_kind().to_string();
    if error.is_retryable() {
        release_retryable_ai_failure(
            ctx,
            emitter,
            owner,
            claimed,
            &message,
            &kind,
            max_attempts,
            now,
        )
        .await
    } else {
        release_permanent_ai_failure(ctx, emitter, owner, claimed, &message, &kind, now).await
    }
}

/// Retryable release with the W15 budget fold; shared by AI errors and
/// transient storage errors reading the article.
#[allow(clippy::too_many_arguments)]
pub(super) async fn release_retryable_ai_failure(
    ctx: &AiDeps,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    claimed: &ClaimedAiResult,
    message: &str,
    kind: &str,
    max_attempts: u32,
    now: OffsetDateTime,
) -> AiTaskOutcome {
    let outcome = match ctx
        .ai_result_repo
        .release_retryable_failure(claimed.id, owner, message, kind, max_attempts, now)
        .await
    {
        Ok(outcome) if outcome.released => outcome,
        Ok(_) => {
            return release_not_applied(emitter, claimed, "retryable", NotApplied::LeaseLost).await;
        }
        Err(error) => {
            return release_not_applied(emitter, claimed, "retryable", NotApplied::Storage(&error))
                .await;
        }
    };
    // W15 §3：预算耗尽时 release SQL 已折叠进 permanent_failed——事件升 error
    // 级并标记 budget_exhausted，summary 计入永久失败而非"将重试"。
    let exhausted = outcome.exhausted;
    emitter
        .emit(
            "ai_failed",
            if exhausted { "error" } else { "warn" },
            Some("article_ai_result"),
            Some(claimed.id),
            message,
            Some(json!({
                "article_id": claimed.article_id,
                "article_ai_result_id": claimed.id,
                "error_kind": kind,
                "retryable": !exhausted,
                "budget_exhausted": exhausted,
            })),
        )
        .await;
    AiTaskOutcome {
        storage_failed: false,
        article_ai_result_id: claimed.id,
        article_id: claimed.article_id,
        status: if exhausted {
            AiTaskStatus::PermanentFailed
        } else {
            AiTaskStatus::RetryableFailed
        },
        article_advance: None,
        error_kind: Some(kind.to_string()),
    }
}

pub(super) async fn release_permanent_ai_failure(
    ctx: &AiDeps,
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
        Ok(true) => {}
        Ok(false) => {
            return release_not_applied(emitter, claimed, "permanent", NotApplied::LeaseLost).await;
        }
        Err(error) => {
            return release_not_applied(emitter, claimed, "permanent", NotApplied::Storage(&error))
                .await;
        }
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
        storage_failed: false,
        article_ai_result_id: claimed.id,
        article_id: claimed.article_id,
        status: AiTaskStatus::PermanentFailed,
        article_advance: None,
        error_kind: Some(kind.to_string()),
    }
}

fn ai_task_status_str(status: &AiTaskStatus) -> &'static str {
    match status {
        AiTaskStatus::Succeeded => "succeeded",
        AiTaskStatus::Filtered => "filtered",
        AiTaskStatus::RetryableFailed => "retryable_failed",
        AiTaskStatus::PermanentFailed => "permanent_failed",
        AiTaskStatus::LeaseLost => "lease_lost",
        AiTaskStatus::ReleaseFailed => "release_failed",
    }
}
