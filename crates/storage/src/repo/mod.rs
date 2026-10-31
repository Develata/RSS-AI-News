pub mod article;
pub mod article_ai_result;
mod article_ai_result_impl;
mod article_ai_result_sql;
pub mod decision_result;
mod decision_result_impl;
mod decision_result_sql;
pub mod feed_entry;
mod feed_entry_impl;
mod feed_entry_sql;
pub mod feed_source;
mod feed_source_impl;
mod feed_source_sql;
pub mod publish_item;
pub mod publish_record;
mod publish_record_impl;
mod publish_record_sql;
pub mod raw_artifact;
pub mod reindex_job;
mod reindex_job_impl;
mod reindex_job_impl_pg;
mod reindex_job_impl_sqlite;
mod reindex_job_sql;
pub mod rule_version;
pub mod run_event;

/// SQLite window bound for TEXT `created_at` columns: UTC truncated to whole
/// seconds as `YYYY-MM-DD HH:MM:SS`, the `CURRENT_TIMESTAMP` layout. SQL
/// compares it with `replace(substr(created_at, 1, 19), 'T', ' ')`, which maps
/// both `CURRENT_TIMESTAMP` rows and sqlx's UTC RFC3339 rows to the same
/// layout, so whole-second (e.g. midnight) windows compare instants exactly.
/// SQLite's `datetime()` is avoided: 3.46 rounds `23:59:59.999999` into the
/// next day.
pub(crate) fn sqlite_utc_seconds(value: time::OffsetDateTime) -> String {
    let value = value.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    )
}

pub use article::{
    ArticleAiTaskCandidate, ArticleContentHash, ArticleInsertOutcome, ArticleRepo,
    ArticleRepository, BackfillArticleCandidate, ContentHashReindexCandidate, NewArticle,
    UpdateContentHashOutcome,
};
pub use article_ai_result::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepo, ArticleAiResultRepository,
    ClaimedAiResult, InsertPendingOutcome, NewAiResult, ReleaseSuccessOutcome,
};
pub use decision_result::{
    ClaimedDecision, DecisionArticle, DecisionAttemptRecord, DecisionExecution, DecisionFailure,
    DecisionFailureMode, DecisionIdentity, DecisionRawRecord, DecisionResultRepo,
    DecisionResultRepository, DecisionResultState, DecisionSuccess,
};
pub use feed_entry::{
    ClaimedFeedEntry, FeedEntry, FeedEntryInsertOutcome, FeedEntryRepo, FeedEntryRepository,
    LinkHashReindexCandidate, NewFeedEntry, RecentFeedEntry, RecentFeedEntryFilter,
    RecentFeedEntryRepository, ResetFailedFilter, ResetFailedOutcome, UpdateLinkHashOutcome,
};
pub use feed_source::{
    FeedSourceRepo, FeedSourceRepository, LeaseGuardedWriteOutcome, RecentFeedSourceHealth,
    RecentFeedSourceHealthRepository,
};
pub use publish_item::{
    FreezeSnapshotItem, FreezeSnapshotOutcome, FreezeSnapshotStatus, PublishCandidateRow,
    PublishItemRepo, PublishItemRepository,
};
pub use publish_record::{
    ClaimedPublishRecord, NewPublishRecord, PublishAdvanceExtras, PublishRecord, PublishRecordRepo,
    PublishRecordRepository, PublishState, PublishTimestampField, TerminalAdvanceOutcome,
    TerminalAdvanceStatus,
};
pub use raw_artifact::{NewRawArtifact, RawArtifactRepo, RawArtifactRepository};
pub use reindex_job::{
    ClaimedReindexJob, FinishReindexTxOutcome, ReindexJob, ReindexJobRepo, ReindexJobRepository,
    StartReindexTxOutcome,
};
pub use rule_version::{ConfigRotation, RuleVersion, RuleVersionRepo, RuleVersionRepository};
pub use run_event::{NewRunEvent, RunEventRepo, RunEventRepository};
