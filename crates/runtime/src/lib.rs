//! Flow coordination through storage-backed state transitions.

pub mod artifact;
pub mod context;
pub mod doctor;
pub mod error;
pub mod events;
pub mod flows;

pub use context::{
    AiDeps, BackfillDeps, ExtractDeps, IngestDeps, PublishDeps, RebuildReportDeps, ReindexDeps,
    RunMeta,
};
pub use error::RuntimeError;
pub use flows::{
    AiProcessSummary, AiRunFlow, AiRunOptions, AiRunSummary, AiTaskOutcome, AiTaskStatus,
    BackfillAiOptions, BackfillAiSummary, BackfillExtractOptions, BackfillExtractSummary,
    BackfillFlow, CategoryPlan, CategoryStats, DEFAULT_RECENT_ENTRIES_LIMIT, DecisionRunDeps,
    DecisionRunFlow, DecisionRunOptions, DecisionRunSettings, DecisionRunSummary,
    EXTRACT_WAVE_MARGIN_SECONDS, ExtractEntryOutcome, ExtractEntryStatus, ExtractFlow,
    ExtractOptions, ExtractSummary, IngestFlow, IngestOptions, IngestSourceOutcome,
    IngestSourceStatus, IngestSummary, MAX_RECENT_ENTRIES_LIMIT, MAX_RECENT_SOURCE_HEALTH_ROWS,
    PublishFlow, PublishFreezeOptions, PublishFreezeOutcome, PublishFreezeStatus,
    PublishInitOptions, PublishInitOutcome, PublishRemoteBatchItemOptions,
    PublishRemoteBatchOptions, PublishRemoteBatchOutcome, PublishRemoteOptions,
    PublishRemoteOutcome, PublishRemoteStatus, PublishRenderOptions, PublishRenderOutcome,
    PublishRenderStatus, PublishStoreLocalOptions, PublishStoreLocalOutcome,
    PublishStoreLocalStatus, RebuildReportFlow, RebuildReportOptions, RecentEntriesFlow,
    RecentEntriesOptions, RecentEntriesResult, RecentSourceHealth, ReindexAbortOutcome,
    ReindexFlow, ReindexOptions, ReindexSummary, ReindexTarget, TaskGenSummary,
    ai_lease_budget_seconds, extract_lease_budget_seconds, fit_extract_batch_to_lease,
};
pub use flows::{
    REPLAY_FEED_SAMPLE_LIMIT, REPLAY_PLACEHOLDER_BASE_URI, ReplayBaseUri, ReplayDiff, ReplayFlow,
    ReplayFlowDeps, ReplayKind, ReplayOptions, ReplayOutcome, ReplayParsed, ReplaySelector,
};
