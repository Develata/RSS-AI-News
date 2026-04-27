//! Flow coordination through storage-backed state transitions.

pub mod artifact;
pub mod context;
pub mod error;
pub mod events;
pub mod flows;

pub use context::{RunContext, RunContextDeps};
pub use error::RuntimeError;
pub use flows::{
    AiProcessSummary, AiRunFlow, AiRunOptions, AiRunSummary, AiTaskOutcome, AiTaskStatus,
    ExtractEntryOutcome, ExtractEntryStatus, ExtractFlow, ExtractOptions, ExtractSummary,
    IngestFlow, IngestOptions, IngestSourceOutcome, IngestSourceStatus, IngestSummary,
    TaskGenSummary,
};
