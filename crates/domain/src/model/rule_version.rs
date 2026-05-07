use time::OffsetDateTime;

use crate::state::RuleVersionStatus;

#[derive(Debug, Clone)]
pub struct RuleVersion {
    pub id: i64,
    pub kind: String,
    pub version_tag: String,
    pub description: String,
    pub payload_sha256: String,
    pub status: RuleVersionStatus,
    pub retired_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}
