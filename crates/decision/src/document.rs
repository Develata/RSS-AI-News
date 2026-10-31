//! `answers_json` (ADR 0011 §8): normalised answers made self-describing with
//! the question definitions, so a stored row reads without the config it was
//! produced under.

use serde_json::{Map, Value, json};

use crate::{
    question::{QuestionKind, QuestionSpec, normalize},
    response::{DecisionAnswer, QuestionAnswer, SpreadConfidence, SpreadKind},
};

/// Bump when the document shape changes; readers keep old versions.
pub const ANSWERS_SCHEMA: u64 = 1;

/// `{"schema":1,"answers":[…]}` in question order. Choice probabilities are
/// keyed by option label, score probabilities are indexed by level. `None`
/// when the answers do not line up with the questions (ids, order or types),
/// which a validated response never produces.
pub fn answers_document(questions: &[QuestionSpec], answers: &[QuestionAnswer]) -> Option<Value> {
    let questions = normalize(questions);
    if questions.len() != answers.len() {
        return None;
    }
    let entries = questions
        .iter()
        .zip(answers)
        .map(|(question, answer)| entry(question, answer))
        .collect::<Option<Vec<_>>>()?;
    Some(json!({ "schema": ANSWERS_SCHEMA, "answers": entries }))
}

fn entry(question: &QuestionSpec, answer: &QuestionAnswer) -> Option<Value> {
    if question.id != answer.id {
        return None;
    }
    let mut object = Map::new();
    object.insert("id".to_string(), json!(answer.id));
    object.insert("type".to_string(), json!(question.kind.type_name()));
    match (&question.kind, &answer.answer) {
        (
            QuestionKind::Noul { .. },
            DecisionAnswer::Noul {
                p_yes,
                top_probability,
            },
        ) => {
            object.insert("p_yes".to_string(), json!(p_yes));
            object.insert("top_probability".to_string(), json!(top_probability));
        }
        (
            QuestionKind::Choice { options },
            DecisionAnswer::Choice {
                label,
                probabilities,
                spread_confidence,
                top_probability,
            },
        ) => {
            if options.len() != probabilities.len() {
                return None;
            }
            let by_label: Map<String, Value> = options
                .iter()
                .zip(probabilities)
                .map(|(option, p)| (option.label.clone(), json!(p)))
                .collect();
            object.insert("label".to_string(), json!(label));
            object.insert("probabilities".to_string(), Value::Object(by_label));
            object.insert("spread_confidence".to_string(), spread(spread_confidence));
            object.insert("top_probability".to_string(), json!(top_probability));
        }
        (
            QuestionKind::Score { levels },
            DecisionAnswer::Score {
                expected,
                probabilities,
                spread_confidence,
                top_probability,
            },
        ) => {
            if levels.len() != probabilities.len() {
                return None;
            }
            object.insert("expected".to_string(), json!(expected));
            object.insert("probabilities".to_string(), json!(probabilities));
            object.insert("spread_confidence".to_string(), spread(spread_confidence));
            object.insert("top_probability".to_string(), json!(top_probability));
        }
        _ => return None,
    }
    Some(Value::Object(object))
}

fn spread(confidence: &SpreadConfidence) -> Value {
    let kind = match confidence.kind {
        SpreadKind::JevChoice => "jev_choice",
        SpreadKind::JevScore => "jev_score",
        SpreadKind::LayaEntropy => "laya_entropy",
    };
    json!({ "kind": kind, "value": confidence.value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::ChoiceOption;

    fn questions() -> Vec<QuestionSpec> {
        vec![
            QuestionSpec {
                id: "q_ad".to_string(),
                instructions: "Ad?".to_string(),
                kind: QuestionKind::Noul {
                    yes: None,
                    no: None,
                },
            },
            QuestionSpec {
                id: "q_kind".to_string(),
                instructions: "Kind?".to_string(),
                kind: QuestionKind::Choice {
                    options: vec![
                        ChoiceOption {
                            label: "paper".to_string(),
                            description: None,
                        },
                        ChoiceOption {
                            label: "other".to_string(),
                            description: None,
                        },
                    ],
                },
            },
            QuestionSpec {
                id: "q_level".to_string(),
                instructions: "Level?".to_string(),
                kind: QuestionKind::Score {
                    levels: vec!["low".to_string(), "high".to_string()],
                },
            },
        ]
    }

    fn answers() -> Vec<QuestionAnswer> {
        let spread = |kind| SpreadConfidence { kind, value: 0.5 };
        vec![
            QuestionAnswer {
                id: "q_ad".to_string(),
                answer: DecisionAnswer::Noul {
                    p_yes: 0.25,
                    top_probability: None,
                },
            },
            QuestionAnswer {
                id: "q_kind".to_string(),
                answer: DecisionAnswer::Choice {
                    label: "paper".to_string(),
                    probabilities: vec![0.75, 0.25],
                    spread_confidence: spread(SpreadKind::JevChoice),
                    top_probability: None,
                },
            },
            QuestionAnswer {
                id: "q_level".to_string(),
                answer: DecisionAnswer::Score {
                    expected: 0.5,
                    probabilities: vec![0.5, 0.5],
                    spread_confidence: spread(SpreadKind::LayaEntropy),
                    top_probability: Some(0.5),
                },
            },
        ]
    }

    // Fixed shape: a change here changes what stored rows mean.
    const DOCUMENT: &str = r#"{"answers":[{"id":"q_ad","p_yes":0.25,"top_probability":null,"type":"noul"},{"id":"q_kind","label":"paper","probabilities":{"other":0.25,"paper":0.75},"spread_confidence":{"kind":"jev_choice","value":0.5},"top_probability":null,"type":"choice"},{"expected":0.5,"id":"q_level","probabilities":[0.5,0.5],"spread_confidence":{"kind":"laya_entropy","value":0.5},"top_probability":0.5,"type":"score"}],"schema":1}"#;

    #[test]
    fn document_shape_is_fixed() {
        let document = answers_document(&questions(), &answers()).unwrap();
        assert_eq!(document.to_string(), DOCUMENT);
    }

    #[test]
    fn mismatched_answers_produce_no_document() {
        let mut swapped = answers();
        swapped.swap(0, 1);
        assert!(answers_document(&questions(), &swapped).is_none());
        assert!(answers_document(&questions(), &answers()[..2]).is_none());
    }
}
