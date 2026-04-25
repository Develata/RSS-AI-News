use time::OffsetDateTime;

use crate::state::PublishState;

#[derive(Debug, Clone)]
pub struct PublishRecord {
    pub id: i64,
    pub idempotency_key: String,
    pub category_key: String,
    pub report_date: String,
    pub target_timezone: String,
    pub render_version: i64,
    pub selection_policy_version: i64,
    pub state: PublishState,
    pub snapshot_frozen_at: Option<OffsetDateTime>,
    pub rendered_at: Option<OffsetDateTime>,
    pub local_stored_at: Option<OffsetDateTime>,
    pub remote_published_at: Option<OffsetDateTime>,
    pub local_path: Option<String>,
    pub remote_target: Option<String>,
    pub commit_sha: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub last_error_kind: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}
