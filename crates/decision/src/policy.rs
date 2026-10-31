//! `policy_version`: what a category asks, as a stable hash (ADR 0010 §6).
//!
//! Canonical form: compact JSON over the normalised questions, object keys in
//! lexicographic order, arrays in configuration order, then SHA-256 as
//! lowercase hex.
//! Keys are inserted in sorted order so the bytes are the same whether
//! `serde_json::Map` is a BTreeMap or (with `preserve_order`) an IndexMap.
//! Provider, model and `laya_max_len` are execution parameters, not policy.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::question::{QuestionKind, QuestionSpec, normalize};

/// Bump when the canonical form itself changes.
pub const POLICY_SCHEMA: u64 = 1;

pub fn canonical_policy(questions: &[QuestionSpec], max_state_chars: u32) -> String {
    let questions: Vec<Value> = normalize(questions)
        .iter()
        .map(canonical_question)
        .collect();
    let mut root = Map::new();
    root.insert("max_state_chars".to_string(), json!(max_state_chars));
    root.insert("policy_schema".to_string(), json!(POLICY_SCHEMA));
    root.insert("questions".to_string(), Value::Array(questions));
    root.insert("state_shape".to_string(), json!(["title", "body"]));
    Value::Object(root).to_string()
}

pub fn policy_version(questions: &[QuestionSpec], max_state_chars: u32) -> String {
    hex::encode(Sha256::digest(
        canonical_policy(questions, max_state_chars).as_bytes(),
    ))
}

fn canonical_question(question: &QuestionSpec) -> Value {
    let mut object = Map::new();
    object.insert("id".to_string(), json!(question.id));
    object.insert("instructions".to_string(), json!(question.instructions));
    match &question.kind {
        QuestionKind::Noul { yes, no } => {
            if let Some(no) = no {
                object.insert("no".to_string(), json!(no));
            }
            object.insert("type".to_string(), json!("noul"));
            if let Some(yes) = yes {
                object.insert("yes".to_string(), json!(yes));
            }
        }
        QuestionKind::Choice { options } => {
            let options: Vec<Value> = options
                .iter()
                .map(|option| {
                    let mut entry = Map::new();
                    entry.insert("description".to_string(), json!(option.description));
                    entry.insert("label".to_string(), json!(option.label));
                    Value::Object(entry)
                })
                .collect();
            object.insert("options".to_string(), Value::Array(options));
            object.insert("type".to_string(), json!("choice"));
        }
        QuestionKind::Score { levels } => {
            object.insert("levels".to_string(), json!(levels));
            object.insert("type".to_string(), json!("score"));
        }
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::ChoiceOption;

    fn sample() -> Vec<QuestionSpec> {
        vec![
            QuestionSpec {
                id: "q_marketing".to_string(),
                instructions: "这篇是否主要是厂商营销或公关稿？".to_string(),
                kind: QuestionKind::Noul {
                    yes: Some("营销".to_string()),
                    no: Some("报道".to_string()),
                },
            },
            QuestionSpec {
                id: "q_kind".to_string(),
                instructions: "这篇属于哪一类？".to_string(),
                kind: QuestionKind::Choice {
                    options: vec![
                        ChoiceOption {
                            label: "paper".to_string(),
                            description: Some("研究论文".to_string()),
                        },
                        ChoiceOption {
                            label: "other".to_string(),
                            description: None,
                        },
                    ],
                },
            },
            QuestionSpec {
                id: "q_importance".to_string(),
                instructions: "重要性？".to_string(),
                kind: QuestionKind::Score {
                    levels: vec!["琐碎".to_string(), "重大".to_string()],
                },
            },
        ]
    }

    fn bare() -> [QuestionSpec; 1] {
        [QuestionSpec {
            id: "q_a".to_string(),
            instructions: "Is it new?".to_string(),
            kind: QuestionKind::Noul {
                yes: None,
                no: None,
            },
        }]
    }

    // Fixed vectors: a change here changes every stored policy_version. The
    // hashes were cross-checked with `printf %s '<json>' | sha256sum`.
    const SAMPLE_JSON: &str = r#"{"max_state_chars":6000,"policy_schema":1,"questions":[{"id":"q_marketing","instructions":"这篇是否主要是厂商营销或公关稿？","no":"报道","type":"noul","yes":"营销"},{"id":"q_kind","instructions":"这篇属于哪一类？","options":[{"description":"研究论文","label":"paper"},{"description":null,"label":"other"}],"type":"choice"},{"id":"q_importance","instructions":"重要性？","levels":["琐碎","重大"],"type":"score"}],"state_shape":["title","body"]}"#;
    const BARE_NOUL_JSON: &str = r#"{"max_state_chars":100,"policy_schema":1,"questions":[{"id":"q_a","instructions":"Is it new?","type":"noul"}],"state_shape":["title","body"]}"#;

    #[test]
    fn canonical_form_is_stable() {
        assert_eq!(canonical_policy(&sample(), 6000), SAMPLE_JSON);
        assert_eq!(canonical_policy(&bare(), 100), BARE_NOUL_JSON);
    }

    #[test]
    fn version_is_the_sha256_of_the_canonical_form() {
        assert_eq!(
            policy_version(&sample(), 6000),
            hex::encode(Sha256::digest(SAMPLE_JSON.as_bytes()))
        );
        assert_eq!(policy_version(&sample(), 6000), SAMPLE_SHA256);
        assert_eq!(policy_version(&bare(), 100), BARE_NOUL_SHA256);
    }

    #[test]
    fn surrounding_whitespace_does_not_change_the_version() {
        let mut padded = sample();
        padded[0].id = format!(" {} ", padded[0].id);
        if let QuestionKind::Choice { options } = &mut padded[1].kind {
            options[0].label = "paper\n".to_string();
            options[1].description = Some("  ".to_string());
        }
        assert_eq!(canonical_policy(&padded, 6000), SAMPLE_JSON);
    }

    #[test]
    fn any_policy_change_changes_the_version() {
        let base = policy_version(&sample(), 6000);
        assert_ne!(policy_version(&sample(), 6001), base);
        let mut reordered = sample();
        reordered.swap(0, 1);
        assert_ne!(policy_version(&reordered, 6000), base);
        let mut described = sample();
        if let QuestionKind::Choice { options } = &mut described[1].kind {
            options[1].description = Some("其它".to_string());
        }
        assert_ne!(policy_version(&described, 6000), base);
    }

    const SAMPLE_SHA256: &str = "0d302268de2a7a19cea1bcb51ae6774f0a0b3194dc359a9d92394f284c51667a";
    const BARE_NOUL_SHA256: &str =
        "d282388a37a4f31d61bdea1c80df09d3c4a4080364af1ed6b917e6b48f88579e";
}
