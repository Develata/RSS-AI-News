use time::OffsetDateTime;

use crate::state::ArtifactKind;

#[derive(Debug, Clone)]
pub struct RawArtifact {
    pub id: i64,
    pub kind: ArtifactKind,
    pub artifact_key: String,
    pub content_encoding: String,
    pub storage_kind: String,
    pub inline_body: Option<Vec<u8>>,
    pub file_path: Option<String>,
    pub byte_size: i64,
    pub sha256: String,
    pub retention_policy: String,
    pub expires_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}
