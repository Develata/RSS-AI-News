use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub struct RuleVersion {
    pub id: i64,
    pub kind: String,
    pub version_tag: String,
    pub description: String,
    pub payload_sha256: String,
    pub retired_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}
