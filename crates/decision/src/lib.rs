//! Typed-decision capability (ADR 0010): ask TypeSafe Jev or a self-hosted
//! laya-server fixed-type questions (yes/no, choice, score) about an article.
//!
//! It assists the generative summary pipeline and never replaces it. The crate
//! is self-contained so the feature can be removed whole: the config crate only
//! passes the raw sections through, and validate-config calls
//! [`config::validate_sections`]. `decision-run` (ADR 0011) is the only flow
//! that calls a provider.

pub mod client;
pub mod config;
pub mod document;
pub mod error;
pub mod policy;
pub mod provider;
pub mod question;
pub mod replay;
pub mod response;

pub use client::{
    ClientSettings, DecisionClient, DecisionRequest, Exchange, RawInfo, SentState, SystemOneClient,
    Withheld,
};
pub use config::{
    CategorySection, DecisionSettings, RawResponsePolicy, STORAGE_DEADLINE_SECONDS, SectionIssue,
    ValidationReport, Warning, min_lease_seconds, questions_from_section, validate_sections,
};
pub use document::{ANSWERS_SCHEMA, answers_document};
pub use error::DecisionError;
pub use policy::{canonical_policy, policy_version, questions_from_canonical};
pub use provider::{Provider, execution_key};
pub use question::{ChoiceOption, Issue, QuestionKind, QuestionSpec};
pub use replay::revalidate;
pub use response::{
    DecisionAnswer, DecisionResponse, Execution, QuestionAnswer, SpreadConfidence, SpreadKind,
    StateTruncation, Usage,
};
