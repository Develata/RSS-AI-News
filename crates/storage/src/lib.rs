//! Persistence layer: SQLite-first with PostgreSQL as replacement target.
//!
//! Owns migrations, repository traits and SQLite adapters.

pub mod error;
pub mod lease;
pub mod migrate;
pub mod pool;
pub mod repo;

pub use error::{StorageError, classify_db_error};
pub use lease::{ClaimRequest, ReleaseOutcome, build_owner_id, lease_expires_at};
pub use migrate::run_migrations;
pub use pool::{StoragePool, build_pg_pool, build_sqlite_pool};
pub use repo::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepo, ArticleAiResultRepository,
    ArticleAiTaskCandidate, ArticleInsertOutcome, ArticleRepo, ArticleRepository,
    BackfillArticleCandidate, ClaimedAiResult, ClaimedFeedEntry, ClaimedPublishRecord,
    ClaimedReindexJob, ContentHashReindexCandidate, FeedEntry, FeedEntryRepo, FeedEntryRepository,
    FeedSourceRepo, FeedSourceRepository, FinishReindexTxOutcome, FreezeSnapshotItem,
    FreezeSnapshotOutcome, FreezeSnapshotStatus, InsertPendingOutcome, LeaseGuardedWriteOutcome,
    LinkHashReindexCandidate, NewAiResult, NewArticle, NewFeedEntry, NewPublishRecord,
    NewRawArtifact, NewRunEvent, PublishAdvanceExtras, PublishCandidateRow, PublishItemRepo,
    PublishItemRepository, PublishRecord, PublishRecordRepo, PublishRecordRepository, PublishState,
    PublishTimestampField, RawArtifactRepo, RawArtifactRepository, ReindexJob, ReindexJobRepo,
    ReindexJobRepository, ReleaseSuccessOutcome, ResetFailedFilter, ResetFailedOutcome,
    RuleVersion, RuleVersionRepo, RuleVersionRepository, RunEventRepo, RunEventRepository,
    StartReindexTxOutcome, TerminalAdvanceOutcome, TerminalAdvanceStatus, UpdateContentHashOutcome,
};
