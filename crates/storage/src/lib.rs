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
    AiSuccessOutcome, ArticleAiResultRepository, ClaimedAiResult, ClaimedFeedEntry,
    ClaimedPublishRecord, FeedEntry, FeedEntryRepository, FeedSourceRepository, NewAiResult,
    NewFeedEntry, NewPublishRecord, PublishAdvanceExtras, PublishRecord, PublishRecordRepository,
    PublishState, PublishTimestampField, RuleVersionRepository, SqliteArticleAiResultRepo,
    SqliteFeedEntryRepo, SqliteFeedSourceRepo, SqlitePublishRecordRepo, SqliteRuleVersionRepo,
};
