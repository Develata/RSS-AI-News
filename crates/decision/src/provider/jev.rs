//! TypeSafe Jev (docs.typesafe.ai /api, /models, /confidence, 2026-10-17).

use serde_json::Value;

use super::{
    WireParams, encode_questions, label_index, option_labels, pair_answers, type_mismatch,
};
use crate::{
    provider::Provider,
    question::{Issue, QuestionKind, QuestionSpec},
    response::{
        DecisionAnswer, DecisionResponse, Execution, SpreadConfidence, SpreadKind, StateTruncation,
        Usage, WireAnswer, WireResponse, distribution, expectation, is_argmax, required, unit,
    },
};

/// Half a unit of the precision Jev returns. Not documented; the official
/// examples and the 2026-10-31 live probe of `jev-1.13.0` both carry two
/// decimals (ADR 0010 §4).
pub(super) const EPS: f64 = 5e-3;

const ALIASES: [&str; 2] = ["jev-latest", "jev-preview"];
const MAX_SCORE_LEVELS: usize = 10;
/// OpenRouter's System One API reports its own id for the serving model,
/// `typesafe/jev-1.14-20261014`, where TypeSafe reports `jev-…`
/// (openrouter.ai/docs/guides/community/typesafe-sdk, read 2026-11-17).
const OPENROUTER_PREFIX: &str = "typesafe/";

/// Whether `reported` names the model that `requested` asked for. An alias
/// accepts any Jev model. A pin accepts itself, or OpenRouter's id for the
/// same name with an optional `-YYYYMMDD` release date: `jev-1.13` matches
/// `typesafe/jev-1.13-20260917`. `jev-1.13.0` does not, because mapping
/// TypeSafe's version onto OpenRouter's would be a guess.
fn model_matches(requested: &str, reported: &str) -> bool {
    let bare = reported.strip_prefix(OPENROUTER_PREFIX);
    if ALIASES.contains(&requested) {
        return bare.unwrap_or(reported).starts_with("jev-");
    }
    reported == requested
        || bare.is_some_and(|bare| {
            bare.strip_prefix(requested).is_some_and(|rest| {
                rest.is_empty()
                    || rest.strip_prefix('-').is_some_and(|date| {
                        date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
        })
}

pub(super) fn check_model(model: &str) -> Result<bool, String> {
    if !model.starts_with("jev-") || model.len() <= "jev-".len() {
        return Err("Jev model must be a jev-… id such as jev-1.13.0".to_string());
    }
    Ok(ALIASES.contains(&model))
}

pub(super) fn validate(questions: &[QuestionSpec], prefix: &str, issues: &mut Vec<Issue>) {
    for (index, question) in questions.iter().enumerate() {
        if let QuestionKind::Score { levels } = &question.kind
            && levels.len() > MAX_SCORE_LEVELS
        {
            issues.push(Issue::new(
                format!("{prefix}[{index}].levels"),
                format!("Jev accepts at most {MAX_SCORE_LEVELS} levels"),
            ));
        }
    }
}

pub(super) fn encode(params: &WireParams<'_>, state: Value, questions: &[QuestionSpec]) -> Value {
    serde_json::json!({
        "state": state,
        "model": params.model,
        "questions": encode_questions(questions, &Value::Null),
    })
}

pub(super) fn decode(
    params: &WireParams<'_>,
    questions: &[QuestionSpec],
    wire: WireResponse,
) -> Result<DecisionResponse, String> {
    if !model_matches(params.model, &wire.model) {
        return Err("reported model does not match the requested model".to_string());
    }
    let answers = pair_answers(questions, wire.answers, decode_answer)?;
    Ok(DecisionResponse {
        execution: Execution {
            provider: Provider::Jev,
            requested_model: params.model.to_string(),
            reported_model: wire.model,
            routing_model: None,
        },
        answers,
        usage: Usage {
            input_tokens: wire.usage.input_tokens,
            state: StateTruncation::NotApplicable,
        },
    })
}

fn decode_answer(question: &QuestionSpec, answer: WireAnswer) -> Result<DecisionAnswer, String> {
    match (&question.kind, answer) {
        (
            QuestionKind::Noul { .. },
            WireAnswer::Noul {
                noul, confidence, ..
            },
        ) => {
            // Jev documents no confidence on noul; tolerate one only if sane.
            if let Some(confidence) = confidence {
                unit(confidence, "confidence")?;
            }
            Ok(DecisionAnswer::Noul {
                p_yes: unit(noul, "noul")?,
                top_probability: None,
            })
        }
        (
            QuestionKind::Choice { .. },
            WireAnswer::Choice {
                choice,
                probabilities,
                confidence,
                ..
            },
        ) => {
            let labels = option_labels(question);
            let index = label_index(&labels, &choice)?;
            let probabilities = distribution(probabilities, &labels, EPS)?;
            if !is_argmax(&probabilities, index) {
                return Err("choice is not a most probable option".to_string());
            }
            Ok(DecisionAnswer::Choice {
                label: choice,
                probabilities,
                spread_confidence: SpreadConfidence {
                    kind: SpreadKind::JevChoice,
                    value: required(confidence, "confidence")?,
                },
                top_probability: None,
            })
        }
        (
            QuestionKind::Score { .. },
            WireAnswer::Score {
                score,
                probabilities,
                confidence,
                ..
            },
        ) => {
            let probabilities = distribution(probabilities, &option_labels(question), EPS)?;
            Ok(DecisionAnswer::Score {
                expected: expectation(score, &probabilities, EPS)?,
                probabilities,
                spread_confidence: SpreadConfidence {
                    kind: SpreadKind::JevScore,
                    value: required(confidence, "confidence")?,
                },
                top_probability: None,
            })
        }
        _ => Err(type_mismatch()),
    }
}

#[cfg(test)]
mod tests {
    use super::model_matches;

    #[test]
    fn aliases_accept_any_jev_model_from_typesafe_or_openrouter() {
        for reported in ["jev-1.13.0", "typesafe/jev-1.14-20261014"] {
            assert!(model_matches("jev-latest", reported), "{reported}");
            assert!(model_matches("jev-preview", reported), "{reported}");
        }
        for reported in [
            "gpt-6",
            "openai/jev-1.14",
            "typesafe/gpt-6",
            "~typesafe/jev-latest",
        ] {
            assert!(!model_matches("jev-latest", reported), "{reported}");
        }
    }

    #[test]
    fn pins_accept_only_the_same_name_with_an_optional_openrouter_date() {
        for reported in [
            "jev-1.13",
            "typesafe/jev-1.13",
            "typesafe/jev-1.13-20260917",
        ] {
            assert!(model_matches("jev-1.13", reported), "{reported}");
        }
        for reported in [
            "jev-1.14",
            "typesafe/jev-1.14-20261014",
            "typesafe/jev-1.13.1",
            "typesafe/jev-1.13-2026091",
            "typesafe/jev-1.13-2026091x",
            "typesafe/jev-1.13-20260917-x",
            "other/jev-1.13",
        ] {
            assert!(!model_matches("jev-1.13", reported), "{reported}");
        }
        // TypeSafe's semantic version is not mapped onto OpenRouter's id.
        assert!(model_matches("jev-1.13.0", "jev-1.13.0"));
        assert!(!model_matches("jev-1.13.0", "typesafe/jev-1.13-20260917"));
        assert!(!model_matches("jev-1", "typesafe/jev-1.13-20260917"));
    }
}
