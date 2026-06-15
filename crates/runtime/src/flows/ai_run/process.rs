//! 单条 AI 任务的执行机制：模型链尝试、invoke + 写 artifact + parse + 构造 outcome。
//! release（成功 / 失败回写）在 [`super::release`]。

use std::sync::Arc;

use rss_ai_news_ai::{parse_response, AiError, AiResponse, AiTask, ParsedResponse};
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_storage::{AiSuccessOutcome, ClaimedAiResult};
use serde_json::json;
use time::OffsetDateTime;

use crate::artifact::ArtifactWriter;
use crate::context::RunContext;
use crate::events::RunEventEmitter;

use super::release::{finish_ai_failure, finish_ai_success, release_permanent_ai_failure};
use super::{AiRunOptions, AiTaskOutcome};

pub(super) async fn process_one(
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
    finish_ai_failure(
        &ctx,
        &emitter,
        &owner,
        &claimed,
        error,
        opts.max_attempts,
        &attempts,
    )
    .await
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
pub(super) struct SuccessfulAttempt {
    pub(super) outcome: AiSuccessOutcome,
    pub(super) keep: bool,
    pub(super) raw_artifact_id: Option<i64>,
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

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn clamp_u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
