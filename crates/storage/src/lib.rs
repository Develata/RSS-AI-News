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
    ArticleInsertOutcome, ArticleRepository, ClaimedAiResult, ClaimedFeedEntry,
    ClaimedPublishRecord, FeedEntry, FeedEntryRepository, FeedSourceRepository,
    InsertPendingOutcome, NewAiResult, NewArticle, NewFeedEntry, NewPublishRecord, NewRawArtifact,
    NewRunEvent, PublishAdvanceExtras, PublishRecord, PublishRecordRepository, PublishState,
    PublishTimestampField, RawArtifactRepository, ReleaseSuccessOutcome, RuleVersionRepository,
    RunEventRepository, SqliteArticleAiResultRepo, SqliteArticleRepo, SqliteFeedEntryRepo,
    SqliteFeedSourceRepo, SqlitePublishRecordRepo, SqliteRawArtifactRepo, SqliteRuleVersionRepo,
    SqliteRunEventRepo,
};
