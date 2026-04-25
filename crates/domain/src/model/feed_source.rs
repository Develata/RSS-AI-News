use time::OffsetDateTime;

use crate::state::{FeedKind, FeedSourceStatus};

#[derive(Debug, Clone)]
pub struct FeedSource {
    pub id: i64,
    pub category_key: String,
    pub source_key: String,
    pub display_name: String,
    pub feed_url: String,
    pub feed_kind: FeedKind,
    pub status: FeedSourceStatus,
    pub priority: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub last_fetched_at: Option<OffsetDateTime>,
    pub last_success_at: Option<OffsetDateTime>,
    pub consecutive_failures: i64,
    pub last_error: Option<String>,
    pub last_error_kind: Option<String>,
    pub config_version: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}
