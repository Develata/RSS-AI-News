//! Provider adapters (ADR 0010 §3–§4). Each provider is one module plus one
//! `Provider` variant, so either can be removed on its own.

mod jev;
mod laya;

/// Laya `max_len` bounds (laya-server `InferenceRequest.max_len`).
pub(crate) const LAYA_MAX_LEN: std::ops::RangeInclusive<u32> = laya::MAX_LEN;

use serde_json::{Map, Value};

use crate::{
    question::{Issue, QuestionKind, QuestionSpec, check_common, normalize},
    response::{
        DecisionAnswer, DecisionResponse, QuestionAnswer, UniqueMap, WireAnswer, WireResponse,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// TypeSafe Jev, hosted.
    Jev,
    /// laya-server (self-hosted), pinned to the sources cited in ADR 0010.
    Laya,
}

impl Provider {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "jev" => Some(Self::Jev),
            "laya" => Some(Self::Laya),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::Laya => "laya",
        }
    }
}

/// Model name check. `Ok(true)` means a moving alias (answers can change
/// without a config change).
pub fn check_model(provider: Provider, model: &str) -> Result<bool, String> {
    match provider {
        Provider::Jev => jev::check_model(model),
        Provider::Laya => laya::check_model(model).map(|()| false),
    }
}

/// Common rules plus the provider's own limits, on the normalised questions.
/// An empty list means the category does not call the provider, so it is
/// accepted.
pub fn validate_questions(
    provider: Provider,
    questions: &[QuestionSpec],
    prefix: &str,
) -> Vec<Issue> {
    let questions = normalize(questions);
    let mut issues = check_common(&questions, prefix);
    if !questions.is_empty() {
        match provider {
            Provider::Jev => jev::validate(&questions, prefix, &mut issues),
            Provider::Laya => laya::validate(&questions, prefix, &mut issues),
        }
    }
    issues
}

pub(crate) struct WireParams<'a> {
    pub model: &'a str,
    pub laya_max_len: u32,
}

/// The `state` both providers receive. Its compact JSON (keys sorted) is what
/// `SentState::state_sha256` hashes.
pub(crate) fn state_value(title: &str, body: &str) -> Value {
    serde_json::json!({ "title": title, "body": body })
}

/// Execution identity of results (ADR 0011 §4): `jev:{model}` or
/// `laya:{model}:{laya_max_len}`. Deployment address and timing are not part
/// of it.
pub fn execution_key(provider: Provider, model: &str, laya_max_len: u32) -> String {
    match provider {
        Provider::Jev => format!("jev:{model}"),
        Provider::Laya => format!("laya:{model}:{laya_max_len}"),
    }
}

pub(crate) fn encode(
    provider: Provider,
    params: &WireParams<'_>,
    state: Value,
    questions: &[QuestionSpec],
) -> Value {
    match provider {
        Provider::Jev => jev::encode(params, state, questions),
        Provider::Laya => laya::encode(params, state, questions),
    }
}

pub(crate) fn decode(
    provider: Provider,
    params: &WireParams<'_>,
    questions: &[QuestionSpec],
    wire: WireResponse,
) -> Result<DecisionResponse, String> {
    match provider {
        Provider::Jev => jev::decode(params, questions, wire),
        Provider::Laya => laya::decode(params, questions, wire),
    }
}

/// `{"type":…, "instructions":…, "criteria"?:…}` with the provider's choice
/// description encoding for an absent description.
fn encode_question(question: &QuestionSpec, absent_description: &Value) -> Value {
    let mut object = Map::new();
    object.insert("type".to_string(), Value::from(question.kind.type_name()));
    object.insert(
        "instructions".to_string(),
        Value::from(question.instructions.as_str()),
    );
    let criteria = match &question.kind {
        // Each side is optional on its own (Jev docs, Laya `agent.py:810`).
        QuestionKind::Noul { yes, no } => {
            let sides: Map<String, Value> = [("true", yes), ("false", no)]
                .into_iter()
                .filter_map(|(key, text)| Some((key.to_string(), Value::from(text.as_deref()?))))
                .collect();
            (!sides.is_empty()).then_some(Value::Object(sides))
        }
        QuestionKind::Choice { options } => Some(Value::Object(
            options
                .iter()
                .map(|option| {
                    let description = option
                        .description
                        .as_deref()
                        .map(Value::from)
                        .unwrap_or_else(|| absent_description.clone());
                    (option.label.clone(), description)
                })
                .collect(),
        )),
        QuestionKind::Score { levels } => Some(Value::from(levels.clone())),
    };
    if let Some(criteria) = criteria {
        object.insert("criteria".to_string(), criteria);
    }
    Value::Object(object)
}

fn encode_questions(questions: &[QuestionSpec], absent_description: &Value) -> Value {
    Value::Object(
        questions
            .iter()
            .map(|question| {
                (
                    question.id.clone(),
                    encode_question(question, absent_description),
                )
            })
            .collect(),
    )
}

/// Pair answers with questions: every question answered exactly once, with
/// the matching type, and no extra answers.
fn pair_answers(
    questions: &[QuestionSpec],
    mut answers: UniqueMap<WireAnswer>,
    mut decode_one: impl FnMut(&QuestionSpec, WireAnswer) -> Result<DecisionAnswer, String>,
) -> Result<Vec<QuestionAnswer>, String> {
    let mut paired = Vec::with_capacity(questions.len());
    for question in questions {
        let answer = answers
            .take(&question.id)
            .ok_or_else(|| format!("no answer for question {}", question.id))?;
        let answer = decode_one(question, answer)
            .map_err(|reason| format!("question {}: {reason}", question.id))?;
        paired.push(QuestionAnswer {
            id: question.id.clone(),
            answer,
        });
    }
    if !answers.0.is_empty() {
        return Err("response has answers for unknown questions".to_string());
    }
    Ok(paired)
}

fn type_mismatch() -> String {
    "answer type does not match the question".to_string()
}

fn option_labels(question: &QuestionSpec) -> Vec<String> {
    match &question.kind {
        QuestionKind::Choice { options } => {
            options.iter().map(|option| option.label.clone()).collect()
        }
        QuestionKind::Score { levels } => {
            (0..levels.len()).map(|level| level.to_string()).collect()
        }
        QuestionKind::Noul { .. } => Vec::new(),
    }
}

fn label_index(labels: &[String], label: &str) -> Result<usize, String> {
    labels
        .iter()
        .position(|candidate| candidate == label)
        .ok_or_else(|| "choice is not one of the options".to_string())
}
