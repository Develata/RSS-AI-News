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
///
/// **F6-2 N2-a 设计澄清**：早期 W0 设计假设 backfill 可通过 `*_version_id`
/// 复用历史版本行（"留空创建新版本 / 指定 id 复用历史"双语义）。F5-7 落地
/// 后实际语义收敛为：**每次 backfill 必然 bump 一个新版本**，用户通过
/// `--prompt-version-tag` / `--prompt-version-description` / `--model`
/// 命名该新版本。AI 版本元数据（tag / sha256 / description / model）由
/// `runtime::flows::backfill::BackfillAiOptions` 承载，不在本 DTO 上重复
/// 声明 —— 见 `docs/design/internal-dto-contracts.md` §6.3。
///
/// 4 个 `*_version_id` 字段（prompt / output_schema / model / extractor）
/// 已删除，因为：
/// 1. 历史 id 复用语义被弃用（不写新版本会破坏 state-machine §4.4
///    "新版本任务行；不覆盖旧行" 的承诺）
/// 2. 这些字段从未在 runtime 构造或消费（dead-code 风险）
/// 3. CLI 用户难以知晓 id（需先查 DB），tag/description 是更稳的入口
#[derive(Debug, Clone)]
pub struct BackfillRequest {
    pub target: BackfillTarget,
    pub category_filter: Option<String>,
    pub date_range: Option<(String, String)>,
    pub batch_size: u32,
    pub dry_run: bool,
}
