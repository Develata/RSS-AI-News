//! Replay and backfill DTOs.

use crate::state::{ArtifactKind, BackfillTarget};

/// Selector for the artifact to replay. Enforces the `--key` vs `--id`
/// mutual-exclusion documented in `cli-semantics.md` §4.5 at the type level —
/// it is impossible to construct a `ReplayRequest` with both selectors set or
/// with neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayArtifactSelector {
    /// Look up the artifact by `(kind, artifact_key)`.
    Key(String),
    /// Look up the artifact by primary key.
    Id(i64),
}

/// Replay request from CLI.
#[derive(Debug, Clone)]
pub struct ReplayRequest {
    pub artifact_kind: ArtifactKind,
    pub selector: ReplayArtifactSelector,
    pub dry_run: bool,
}

/// Replay result.
#[derive(Debug, Clone)]
pub struct ReplayResult {
    pub artifact_kind: ArtifactKind,
    pub artifact_key: String,
    pub parsed_output: String,
    pub diff: Option<String>,
    pub errors: Vec<String>,
}

/// Backfill request from CLI.
#[derive(Debug, Clone)]
pub struct BackfillRequest {
    pub target: BackfillTarget,
    pub category_filter: Option<String>,
    pub date_range: Option<(String, String)>,
    pub batch_size: u32,
    pub dry_run: bool,
    pub prompt_version_id: Option<i64>,
    pub output_schema_version_id: Option<i64>,
    pub model_id: Option<String>,
    pub extractor_version_id: Option<i64>,
}
