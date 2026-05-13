//! Persistence layer: SQLite-first with PostgreSQL as replacement target.
//!
//! Owns migrations, repository traits and SQLite adapters.

pub mod error;
pub mod lease;
pub mod migrate;
pub mod pool;
pub mod repo;

pub use error::{StorageError, classify_sqlite_error};
pub use lease::{ClaimRequest, ReleaseOutcome, build_owner_id, lease_expires_at};
pub use migrate::run_migrations;
pub use pool::build_sqlite_pool;
pub use repo::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepository, ArticleAiTaskCandidate,
    ArticleInsertOutcome, ArticleRepository, BackfillArticleCandidate, ClaimedAiResult,
    ClaimedFeedEntry, ClaimedPublishRecord, ClaimedReindexJob, ContentHashReindexCandidate,
    FeedEntry, FeedEntryRepository, FeedSourceRepository, FreezeSnapshotItem,
    FreezeSnapshotOutcome, FreezeSnapshotStatus, InsertPendingOutcome, LinkHashReindexCandidate,
    NewAiResult, NewArticle, NewFeedEntry, NewPublishRecord, NewRawArtifact, NewRunEvent,
    PublishAdvanceExtras, PublishCandidateRow, PublishItemRepository, PublishRecord,
    PublishRecordRepository, PublishState, PublishTimestampField, RawArtifactRepository,
    ReindexJob, ReindexJobRepository, ReleaseSuccessOutcome, ResetFailedFilter, ResetFailedOutcome,
    RuleVersion, RuleVersionRepository, RunEventRepository, SqliteArticleAiResultRepo,
    SqliteArticleRepo, SqliteFeedEntryRepo, SqliteFeedSourceRepo, SqlitePublishItemRepo,
    SqlitePublishRecordRepo, SqliteRawArtifactRepo, SqliteReindexJobRepo, SqliteRuleVersionRepo,
    SqliteRunEventRepo, TerminalAdvanceOutcome, TerminalAdvanceStatus, UpdateContentHashOutcome,
};
