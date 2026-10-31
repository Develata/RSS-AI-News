//! Question specifications shared by every provider (ADR 0010 §2–§3).

use std::collections::HashSet;

/// Project cap on choice options for both providers: Laya's option head budget
/// collapses options beyond about 20, and the cap keeps probability-sum
/// tolerances meaningful for coarse provider precision.
pub const MAX_CHOICE_OPTIONS: usize = 20;

/// Project caps for both providers (Laya's own limits; Jev documents none).
/// They bound the synchronous work per request (ADR 0011 §7).
pub const MAX_QUESTIONS: usize = 50;
pub const MAX_INSTRUCTION_CHARS: usize = 2000;

/// One typed-decision question. Every check, request and hash uses
/// [`QuestionSpec::normalized`], so surrounding whitespace never matters.
/// The id is a stable key; Jev documents that ids are not shown to the model
/// and Laya's default encoding path does not use them either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionSpec {
    pub id: String,
    pub instructions: String,
    pub kind: QuestionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionKind {
    /// Yes / no. `yes` / `no` optionally describe each side, independently.
    Noul {
        yes: Option<String>,
        no: Option<String>,
    },
    /// Exactly one of the options.
    Choice { options: Vec<ChoiceOption> },
    /// Ordered levels, lowest first; a level's number is its index.
    Score { levels: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOption {
    pub label: String,
    pub description: Option<String>,
}

impl QuestionSpec {
    /// The canonical form every check, request and hash uses: strings trimmed,
    /// blank optional texts dropped. The config parser already produces it;
    /// this keeps directly constructed specs equivalent.
    pub fn normalized(&self) -> Self {
        let trim = |value: &str| value.trim().to_string();
        let optional = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let kind = match &self.kind {
            QuestionKind::Noul { yes, no } => QuestionKind::Noul {
                yes: optional(yes),
                no: optional(no),
            },
            QuestionKind::Choice { options } => QuestionKind::Choice {
                options: options
                    .iter()
                    .map(|option| ChoiceOption {
                        label: trim(&option.label),
                        description: optional(&option.description),
                    })
                    .collect(),
            },
            QuestionKind::Score { levels } => QuestionKind::Score {
                levels: levels.iter().map(|level| trim(level)).collect(),
            },
        };
        Self {
            id: trim(&self.id),
            instructions: trim(&self.instructions),
            kind,
        }
    }
}

pub fn normalize(questions: &[QuestionSpec]) -> Vec<QuestionSpec> {
    questions.iter().map(QuestionSpec::normalized).collect()
}

impl QuestionKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }
}

/// A configuration or request problem, found without any network call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub field: String,
    pub message: String,
}

impl Issue {
    pub(crate) fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Provider-independent rules on the normalised questions. `prefix` is the
/// field path of the question list, e.g. `category.decision.questions`.
pub fn validate_common(questions: &[QuestionSpec], prefix: &str) -> Vec<Issue> {
    check_common(&normalize(questions), prefix)
}

/// [`validate_common`] for questions that are already normalised.
pub(crate) fn check_common(questions: &[QuestionSpec], prefix: &str) -> Vec<Issue> {
    let mut issues = Vec::new();
    if questions.len() > MAX_QUESTIONS {
        issues.push(Issue::new(
            prefix,
            format!("at most {MAX_QUESTIONS} questions"),
        ));
    }
    let mut ids = HashSet::new();
    for (index, question) in questions.iter().enumerate() {
        let at = |field: &str| format!("{prefix}[{index}].{field}");
        if !valid_id(&question.id) {
            issues.push(Issue::new(at("id"), "must be non-empty [a-z0-9_]"));
        } else if !ids.insert(question.id.as_str()) {
            issues.push(Issue::new(at("id"), "duplicate question id"));
        }
        if question.instructions.is_empty() {
            issues.push(Issue::new(at("instructions"), "must not be empty"));
        } else if question.instructions.chars().count() > MAX_INSTRUCTION_CHARS {
            // Unicode scalar values, like Laya's Python `str` length.
            issues.push(Issue::new(
                at("instructions"),
                format!("at most {MAX_INSTRUCTION_CHARS} characters"),
            ));
        }
        match &question.kind {
            QuestionKind::Noul { .. } => {}
            QuestionKind::Choice { options } => {
                if !(2..=MAX_CHOICE_OPTIONS).contains(&options.len()) {
                    issues.push(Issue::new(
                        at("options"),
                        format!("needs 2..={MAX_CHOICE_OPTIONS} options"),
                    ));
                }
                check_labels(
                    options.iter().map(|option| option.label.as_str()),
                    &at("options"),
                    &mut issues,
                );
            }
            QuestionKind::Score { levels } => {
                if levels.len() < 2 {
                    issues.push(Issue::new(at("levels"), "needs at least 2 levels"));
                }
                check_labels(
                    levels.iter().map(String::as_str),
                    &at("levels"),
                    &mut issues,
                );
            }
        }
    }
    issues
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn check_labels<'a>(labels: impl Iterator<Item = &'a str>, field: &str, issues: &mut Vec<Issue>) {
    let mut seen = HashSet::new();
    for label in labels {
        if label.is_empty() {
            issues.push(Issue::new(field, "labels must not be empty"));
        } else if !seen.insert(label) {
            issues.push(Issue::new(field, "labels must be unique"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noul(id: &str) -> QuestionSpec {
        QuestionSpec {
            id: id.to_string(),
            instructions: "Is it marketing?".to_string(),
            kind: QuestionKind::Noul {
                yes: None,
                no: None,
            },
        }
    }

    #[test]
    fn accepts_a_well_formed_set() {
        let choice = QuestionSpec {
            id: "q_kind".to_string(),
            instructions: "Which kind?".to_string(),
            kind: QuestionKind::Choice {
                options: vec![
                    ChoiceOption {
                        label: "paper".to_string(),
                        description: None,
                    },
                    ChoiceOption {
                        label: "other".to_string(),
                        description: Some("anything else".to_string()),
                    },
                ],
            },
        };
        assert!(validate_common(&[noul("q_a"), choice], "q").is_empty());
    }

    #[test]
    fn rejects_bad_ids_and_duplicates_but_allows_one_sided_noul_criteria() {
        let mut one_sided = noul("q_b");
        one_sided.kind = QuestionKind::Noul {
            yes: Some("yes".to_string()),
            no: None,
        };
        let issues = validate_common(&[noul("Bad-Id"), noul("q_a"), noul("q_a"), one_sided], "q");
        let fields: Vec<_> = issues.iter().map(|issue| issue.field.as_str()).collect();
        assert_eq!(fields, ["q[0].id", "q[2].id"]);
    }

    #[test]
    fn directly_built_specs_are_checked_after_trimming() {
        let mut blank = noul("q_blank");
        blank.instructions = "   ".to_string();
        let padded = QuestionSpec {
            id: " q_level ".to_string(),
            instructions: "How?".to_string(),
            kind: QuestionKind::Score {
                levels: vec!["a".to_string(), " a ".to_string()],
            },
        };
        let messages: Vec<_> = validate_common(&[blank, padded], "q")
            .into_iter()
            .map(|issue| issue.message)
            .collect();
        assert_eq!(messages, ["must not be empty", "labels must be unique"]);
    }

    #[test]
    fn project_caps_apply_to_every_provider() {
        let mut long = noul("q_long");
        long.instructions = "长".repeat(MAX_INSTRUCTION_CHARS + 1);
        let issues = validate_common(&[long], "q");
        assert_eq!(issues[0].field, "q[0].instructions");
        let many: Vec<_> = (0..=MAX_QUESTIONS)
            .map(|i| noul(&format!("q_{i}")))
            .collect();
        assert_eq!(validate_common(&many, "q")[0].field, "q");
        let fine = "长".repeat(MAX_INSTRUCTION_CHARS);
        let mut edge = noul("q_edge");
        edge.instructions = fine;
        assert!(validate_common(&[edge], "q").is_empty());
    }

    #[test]
    fn rejects_duplicate_or_empty_labels_and_out_of_range_counts() {
        let levels = QuestionSpec {
            id: "q_level".to_string(),
            instructions: "How important?".to_string(),
            kind: QuestionKind::Score {
                levels: vec!["low".to_string(), "low".to_string(), String::new()],
            },
        };
        let too_many = QuestionSpec {
            id: "q_many".to_string(),
            instructions: "Which?".to_string(),
            kind: QuestionKind::Choice {
                options: (0..=MAX_CHOICE_OPTIONS)
                    .map(|index| ChoiceOption {
                        label: format!("o{index}"),
                        description: None,
                    })
                    .collect(),
            },
        };
        let messages: Vec<_> = validate_common(&[levels, too_many], "q")
            .into_iter()
            .map(|issue| issue.message)
            .collect();
        assert_eq!(
            messages,
            [
                "labels must be unique",
                "labels must not be empty",
                "needs 2..=20 options"
            ]
        );
    }
}
