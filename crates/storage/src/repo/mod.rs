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
pub mod replay;
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

pub use article::EntryPersistOutcome;
/// A deadline as stored (lease expiry, raw-response TTL): UTC, rounded up
/// to a whole millisecond. With [`sqlite_instant_cutoff`] rounding the
/// compared `now` down, SQLite's `julianday()` (millisecond precision)
/// compares deadlines exactly and never treats one as passed early; UTC keeps
/// the text parseable by SQLite date functions. Same rule as `feed_entries`
/// (`feed_entry_impl::stored_deadline`, plan/02 §5.3).
pub(crate) fn sqlite_stored_deadline(at: time::OffsetDateTime) -> time::OffsetDateTime {
    let at = at.to_offset(time::UtcOffset::UTC);
    match at.nanosecond() % 1_000_000 {
        0 => at,
        sub_ms => at + time::Duration::nanoseconds(i64::from(1_000_000 - sub_ms)),
    }
}

/// The `now` SQLite deadline predicates compare against: UTC, rounded down
/// to a whole millisecond (see [`sqlite_stored_deadline`]).
pub(crate) fn sqlite_instant_cutoff(now: time::OffsetDateTime) -> time::OffsetDateTime {
    let now = now.to_offset(time::UtcOffset::UTC);
    now - time::Duration::nanoseconds(i64::from(now.nanosecond() % 1_000_000))
}

/// SQLite bounds of a C9 window (R2-S1/S2): `julianday()` resolves source
/// times to whole milliseconds, so both bounds are rounded **inward** — the
/// start up, the end down (UTC) — and an instant before `since` is never
/// admitted because it shares `since`'s millisecond.
pub(crate) fn sqlite_window_bounds(
    window: FreshWindow,
) -> (time::OffsetDateTime, time::OffsetDateTime) {
    (
        sqlite_stored_deadline(window.since),
        sqlite_instant_cutoff(window.until),
    )
}

pub use article::{
    ArticleAiTaskCandidate, ArticleContentHash, ArticleInsertOutcome, ArticleRepo,
    ArticleRepository, BackfillArticleCandidate, ContentHashReindexCandidate, NewArticle,
    UpdateContentHashOutcome,
};
pub use article_ai_result::FreshWindow;
pub use article_ai_result::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepo, ArticleAiResultRepository,
    ClaimedAiResult, InsertPendingOutcome, NewAiResult, ReleaseSuccessOutcome, ShareMark,
    ShareSide, ShareStage,
};
pub use decision_result::{
    ClaimedDecision, DecisionArticle, DecisionAttemptRecord, DecisionExecution, DecisionFailure,
    DecisionFailureMode, DecisionIdentity, DecisionRawRecord, DecisionResultRepo,
    DecisionResultRepository, DecisionResultState, DecisionSuccess, GeneratedPage,
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
    ClaimedReindexJob, FinishReindexTxOutcome, ReindexBatchCheckpoint, ReindexJob, ReindexJobRepo,
    ReindexJobRepository, ResumeReindexOutcome, StartReindexTxOutcome,
};
pub use replay::{
    HtmlReplaySource, ReplayArticleDigest, ReplayProjectionRepo, ReplayProjectionRepository,
};
pub use rule_version::{ConfigRotation, RuleVersion, RuleVersionRepo, RuleVersionRepository};
pub use run_event::{NewRunEvent, RunEventRepo, RunEventRepository};
