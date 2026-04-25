//! Persistence layer: SQLite-first with PostgreSQL as replacement target.
//!
//! Owns migrations, repository traits and SQLite adapters.

pub mod error;
pub mod migrate;
pub mod pool;
pub mod repo;

pub use error::{StorageError, classify_sqlite_error};
pub use migrate::run_migrations;
pub use pool::build_sqlite_pool;
pub use repo::{
    FeedSourceRepository, RuleVersionRepository, SqliteFeedSourceRepo, SqliteRuleVersionRepo,
};
