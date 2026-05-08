//! Domain model types — persistent object representations.
//!
//! These types map 1:1 to database truth-source tables.
//! See docs/design/storage-schema.md for field-level details.

pub mod article;
pub mod article_ai_result;
pub mod feed_entry;
pub mod feed_source;
pub mod publish_item;
pub mod publish_record;
pub mod raw_artifact;
pub mod reindex_job;
pub mod rule_version;
pub mod run_event;

pub use article::Article;
pub use article_ai_result::ArticleAiResult;
pub use feed_entry::FeedEntry;
pub use feed_source::FeedSource;
pub use publish_item::PublishItem;
pub use publish_record::PublishRecord;
pub use raw_artifact::RawArtifact;
pub use reindex_job::ReindexJob;
pub use rule_version::RuleVersion;
pub use run_event::RunEvent;
