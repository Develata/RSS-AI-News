//! laya-server `434e9b3` (backend/server/{schemas,main}.py) running Laya
//! `6d942c9` (laya/agent.py, laya/common.py).

use serde_json::Value;

use super::{
    WireParams, encode_questions, label_index, option_labels, pair_answers, type_mismatch,
};
use crate::{
    provider::Provider,
    question::{Issue, QuestionKind, QuestionSpec},
    response::{
        DecisionAnswer, DecisionResponse, Execution, SpreadConfidence, SpreadKind, StateTruncation,
        Usage, WireAnswer, WireResponse, distribution, expectation, is_argmax, max_of, required,
        unit,
    },
};

/// Laya rounds every probability and confidence to 4 decimals.
pub(super) const EPS: f64 = 5e-5;

/// Explicit checkpoints only. `auto` lets the server pick a checkpoint by
/// input language, which is per-input model routing (non-goals §5.2).
const MODELS: [&str; 3] = ["multilingual", "english", "typed-decisions"];
const MAX_QUESTIONS: usize = 50;
const MAX_LEVELS: usize = 50;
const MAX_INSTRUCTION_CHARS: usize = 2000;
pub(crate) const MAX_LEN: std::ops::RangeInclusive<u32> = 512..=8192;

pub(super) fn check_model(model: &str) -> Result<(), String> {
    if model == "auto" {
        return Err("Laya model must name a checkpoint: auto routes by input language".to_string());
    }
    if MODELS.contains(&model) {
        Ok(())
    } else {
        Err("Laya model must be multilingual, english or typed-decisions".to_string())
    }
}

pub(super) fn validate(questions: &[QuestionSpec], prefix: &str, issues: &mut Vec<Issue>) {
    if questions.len() > MAX_QUESTIONS {
        issues.push(Issue::new(
            prefix,
            format!("Laya accepts at most {MAX_QUESTIONS} questions"),
        ));
    }
    for (index, question) in questions.iter().enumerate() {
        // Python `str` length: Unicode scalar values, not bytes.
        if question.instructions.chars().count() > MAX_INSTRUCTION_CHARS {
            issues.push(Issue::new(
                format!("{prefix}[{index}].instructions"),
                format!("Laya accepts at most {MAX_INSTRUCTION_CHARS} characters"),
            ));
        }
        if let QuestionKind::Score { levels } = &question.kind
            && levels.len() > MAX_LEVELS
        {
            issues.push(Issue::new(
                format!("{prefix}[{index}].levels"),
                format!("Laya accepts at most {MAX_LEVELS} levels"),
            ));
        }
    }
}

/// Exactly the four fields laya-server accepts (`extra="forbid"`), with an
/// explicit `max_len` so the server default never decides truncation.
pub(super) fn encode(params: &WireParams<'_>, state: Value, questions: &[QuestionSpec]) -> Value {
    serde_json::json!({
        "state": state,
        "questions": encode_questions(questions, &Value::from("")),
        "model": params.model,
        "max_len": params.laya_max_len,
    })
}

pub(super) fn decode(
    params: &WireParams<'_>,
    questions: &[QuestionSpec],
    wire: WireResponse,
) -> Result<DecisionResponse, String> {
    if wire.model.is_empty() {
        return Err("model is missing".to_string());
    }
    let routing_model = wire
        .routing
        .and_then(|routing| routing.model)
        .ok_or_else(|| "routing.model is missing".to_string())?;
    if routing_model != params.model {
        return Err("routing.model does not match the requested checkpoint".to_string());
    }
    let truncated = wire
        .usage
        .truncated
        .ok_or_else(|| "usage.truncated is missing".to_string())?;
    let state_tokens_dropped = wire
        .usage
        .state_tokens_dropped
        .ok_or_else(|| "usage.state_tokens_dropped is missing".to_string())?;
    // Options squeezed past the head budget read the same to the model: the
    // answer looks well-formed but cannot be trusted (laya/common.py
    // `collapsed_options`).
    if let Some(collapsed) = wire.usage.options
        && collapsed
            .0
            .iter()
            .any(|(_, stats)| stats.distinct < stats.total)
    {
        return Err("options collapsed past the head budget".to_string());
    }
    let answers = pair_answers(questions, wire.answers, decode_answer)?;
    Ok(DecisionResponse {
        execution: Execution {
            provider: Provider::Laya,
            requested_model: params.model.to_string(),
            reported_model: wire.model,
            routing_model: Some(routing_model),
        },
        answers,
        usage: Usage {
            input_tokens: wire.usage.input_tokens,
            state: StateTruncation::Laya {
                truncated,
                state_tokens_dropped,
            },
        },
    })
}

/// `answer_confidence` is `max(p)` and noul `confidence` is `max(p, 1 − p)`;
/// both are 1-Lipschitz, so after 4-decimal rounding they agree within 2·EPS.
/// The entropy `confidence` of choice / score is range-checked only: it is
/// non-linear and can drift by ~0.007 for 50 options.
fn near_max(value: f64, expected: f64, what: &str) -> Result<f64, String> {
    if (value - expected).abs() <= 2.0 * EPS + 1e-9 {
        Ok(value)
    } else {
        Err(format!("{what} does not match the probabilities"))
    }
}

fn decode_answer(question: &QuestionSpec, answer: WireAnswer) -> Result<DecisionAnswer, String> {
    match (&question.kind, answer) {
        (
            QuestionKind::Noul { .. },
            WireAnswer::Noul {
                noul,
                confidence,
                answer_confidence,
            },
        ) => {
            let p_yes = unit(noul, "noul")?;
            let top = p_yes.max(1.0 - p_yes);
            near_max(required(confidence, "confidence")?, top, "confidence")?;
            let answer_confidence = near_max(
                required(answer_confidence, "answer_confidence")?,
                top,
                "answer_confidence",
            )?;
            Ok(DecisionAnswer::Noul {
                p_yes,
                top_probability: Some(answer_confidence),
            })
        }
        (
            QuestionKind::Choice { .. },
            WireAnswer::Choice {
                choice,
                probabilities,
                confidence,
                answer_confidence,
            },
        ) => {
            let labels = option_labels(question);
            let index = label_index(&labels, &choice)?;
            let probabilities = distribution(probabilities, &labels, EPS)?;
            if !is_argmax(&probabilities, index) {
                return Err("choice is not a most probable option".to_string());
            }
            let answer_confidence = near_max(
                required(answer_confidence, "answer_confidence")?,
                max_of(&probabilities),
                "answer_confidence",
            )?;
            Ok(DecisionAnswer::Choice {
                label: choice,
                probabilities,
                spread_confidence: SpreadConfidence {
                    kind: SpreadKind::LayaEntropy,
                    value: required(confidence, "confidence")?,
                },
                top_probability: Some(answer_confidence),
            })
        }
        (
            QuestionKind::Score { .. },
            WireAnswer::Score {
                score,
                probabilities,
                confidence,
                answer_confidence,
            },
        ) => {
            let probabilities = distribution(probabilities, &option_labels(question), EPS)?;
            let expected = expectation(score, &probabilities, EPS)?;
            let answer_confidence = near_max(
                required(answer_confidence, "answer_confidence")?,
                max_of(&probabilities),
                "answer_confidence",
            )?;
            Ok(DecisionAnswer::Score {
                expected,
                probabilities,
                spread_confidence: SpreadConfidence {
                    kind: SpreadKind::LayaEntropy,
                    value: required(confidence, "confidence")?,
                },
                top_probability: Some(answer_confidence),
            })
        }
        _ => Err(type_mismatch()),
    }
}
