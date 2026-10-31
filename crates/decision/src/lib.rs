//! Typed-decision capability (ADR 0010): ask TypeSafe Jev or a self-hosted
//! laya-server fixed-type questions (yes/no, choice, score) about an article.
//!
//! It assists the generative summary pipeline and never replaces it. The crate
//! is self-contained so the feature can be removed whole: the config crate only
//! passes the raw sections through, and validate-config calls
//! [`config::validate_sections`]. Phase A wires it into no flow.

pub mod client;
pub mod config;
pub mod error;
pub mod policy;
pub mod provider;
pub mod question;
pub mod response;

pub use client::{ClientSettings, DecisionClient, DecisionRequest, SystemOneClient};
pub use config::{
    CategorySection, DecisionSettings, SectionIssue, ValidationReport, Warning,
    questions_from_section, validate_sections,
};
pub use error::DecisionError;
pub use policy::{canonical_policy, policy_version};
pub use provider::Provider;
pub use question::{ChoiceOption, Issue, QuestionKind, QuestionSpec};
pub use response::{
    DecisionAnswer, DecisionResponse, Execution, QuestionAnswer, SpreadConfidence, SpreadKind,
    StateTruncation, Usage,
};
