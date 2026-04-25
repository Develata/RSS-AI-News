use time::OffsetDateTime;

use crate::state::{DedupDecision, FeedEntryState};

#[derive(Debug, Clone)]
pub struct FeedEntry {
    pub id: i64,
    pub source_id: i64,
    pub feed_entry_uid: String,
    pub normalized_link: String,
    pub link_hash: String,
    pub title_raw: String,
    pub summary_raw: Option<String>,
    pub published_at: Option<OffsetDateTime>,
    pub discovered_at: OffsetDateTime,
    pub state: FeedEntryState,
    pub dedup_decision: Option<DedupDecision>,
    pub article_id: Option<i64>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub last_error_kind: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}
