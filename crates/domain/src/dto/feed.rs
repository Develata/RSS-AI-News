//! Feed-stage DTOs.

use time::OffsetDateTime;

use crate::{
    SecretString,
    state::{DedupDecision, FeedKind},
};

/// Request from runtime to feed crate: fetch a feed source.
#[derive(Debug, Clone)]
pub struct FeedFetchRequest {
    pub source_id: i64,
    pub category_key: String,
    pub source_key: String,
    pub feed_url: String,
    pub feed_kind: FeedKind,
    pub rsshub_access_key: Option<SecretString>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub timeout: std::time::Duration,
}

/// Response from feed crate: parsed feed entries.
#[derive(Debug, Clone)]
pub struct FeedFetchResponse {
    pub source_id: i64,
    pub http_status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub not_modified: bool,
    pub entries: Vec<FeedEntryMeta>,
    pub raw_payload_bytes: Option<Vec<u8>>,
}

/// Single entry metadata parsed from a feed.
#[derive(Debug, Clone)]
pub struct FeedEntryMeta {
    pub feed_entry_uid: String,
    pub title_raw: String,
    pub link_raw: String,
    pub summary_raw: Option<String>,
    pub published_at: Option<OffsetDateTime>,
}

/// Dedup result for a single entry.
#[derive(Debug, Clone)]
pub struct DedupResult {
    pub entry_meta: FeedEntryMeta,
    pub normalized_link: String,
    pub link_hash: String,
    pub decision: DedupDecision,
    pub existing_entry_id: Option<i64>,
}
