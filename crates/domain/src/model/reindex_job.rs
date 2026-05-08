use time::OffsetDateTime;

use crate::state::{ReindexJobState, ReindexTarget};

/// Reindex job persistent record. Maps 1:1 to the `reindex_jobs` table
/// documented in `storage-schema.md` §4.10. Lifecycle: see
/// `state-machine.md` §6.
#[derive(Debug, Clone)]
pub struct ReindexJob {
    pub id: i64,
    pub target: ReindexTarget,
    pub rule_version_id: i64,
    pub last_processed_id: Option<i64>,
    pub total_estimated: Option<i64>,
    pub state: ReindexJobState,
    pub error: Option<String>,
    pub aborted_reason: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub attempt_count: i64,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}
