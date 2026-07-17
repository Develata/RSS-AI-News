//! Persistence layer: SQLite-first with PostgreSQL as replacement target.
//!
//! Owns migrations, repository traits and SQLite adapters.

pub mod error;
pub mod lease;
pub mod migrate;
pub mod pool;
pub mod repo;

pub use error::{StorageError, classify_db_error};
pub use lease::{
    ClaimRequest, ReleaseFailureOutcome, ReleaseOutcome, build_owner_id, lease_expires_at,
};
pub use migrate::{
    applied_migration_versions, embedded_migration_versions, ensure_migration_state_exact,
    pending_migration_versions, run_migrations,
};
pub use pool::{StoragePool, build_pg_pool, build_sqlite_pool, build_sqlite_read_only_pool};
pub use repo::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepo, ArticleAiResultRepository,
    ArticleAiTaskCandidate, ArticleInsertOutcome, ArticleRepo, ArticleRepository,
    BackfillArticleCandidate, ClaimedAiResult, ClaimedFeedEntry, ClaimedPublishRecord,
    ClaimedReindexJob, ConfigRotation, ContentHashReindexCandidate, FeedEntry,
    FeedEntryInsertOutcome, FeedEntryRepo, FeedEntryRepository, FeedSourceRepo,
    FeedSourceRepository, FinishReindexTxOutcome, FreezeSnapshotItem, FreezeSnapshotOutcome,
    FreezeSnapshotStatus, InsertPendingOutcome, LeaseGuardedWriteOutcome, LinkHashReindexCandidate,
    NewAiResult, NewArticle, NewFeedEntry, NewPublishRecord, NewRawArtifact, NewRunEvent,
    PublishAdvanceExtras, PublishCandidateRow, PublishItemRepo, PublishItemRepository,
    PublishRecord, PublishRecordRepo, PublishRecordRepository, PublishState, PublishTimestampField,
    RawArtifactRepo, RawArtifactRepository, RecentFeedEntry, RecentFeedEntryFilter,
    RecentFeedEntryRepository, RecentFeedSourceHealth, RecentFeedSourceHealthRepository,
    ReindexJob, ReindexJobRepo, ReindexJobRepository, ReleaseSuccessOutcome, ResetFailedFilter,
    ResetFailedOutcome, RuleVersion, RuleVersionRepo, RuleVersionRepository, RunEventRepo,
    RunEventRepository, StartReindexTxOutcome, TerminalAdvanceOutcome, TerminalAdvanceStatus,
    UpdateContentHashOutcome, UpdateLinkHashOutcome,
};
