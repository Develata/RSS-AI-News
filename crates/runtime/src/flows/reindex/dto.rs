//! reindex flow 的输入 / 输出 DTO。

use rss_ai_news_config::CategoryConfig;
use rss_ai_news_domain::state::ReindexTarget;

#[derive(Debug, Clone)]
pub struct ReindexOptions {
    pub target: ReindexTarget,
    pub batch_size: u32,
    pub categories: Vec<CategoryConfig>,
    pub new_rule_version_tag: String,
    pub new_rule_version_description: String,
    pub new_rule_version_sha256: String,
}

#[derive(Debug, Clone, Default)]
pub struct ReindexSummary {
    pub new_rule_version_id: i64,
    /// F15-7：每次 reindex 由 `start_reindex_tx` 同事务创建的 reindex_jobs
    /// 行 id。CLI 通过该字段把 job_id 暴露给用户（`reindex --abort <job_id>`
    /// 寻址用）；F15-9 finish TX 用此 id 推进跨表激活。
    ///
    /// **dry-run** 模式下不创建 rule_versions / reindex_jobs；此时
    /// `new_rule_version_id = 0` 且 `reindex_job_id = 0`。
    pub reindex_job_id: i64,
    pub scanned: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub conflict_skipped: u32,
    pub archived: u32,
    pub errors: u32,
}

/// [`super::ReindexFlow::abort`] 返回值。`aborted=true` 表示 storage 真把状态从
/// `pending`/`running` 推到 `aborted`；`aborted=false` 表示 job 已处于
/// terminal 状态（completed/failed/aborted）或不存在，不算错误——CLI 据此
/// 给出 "no active job to abort" 的 user-friendly 反馈。
#[derive(Debug, Clone)]
pub struct ReindexAbortOutcome {
    pub job_id: i64,
    pub aborted: bool,
    /// 仅当 `aborted=true` 且 job 存在时填入 job 的 target；CLI 用于在
    /// pretty 输出里打回执（"Aborted job 42 (target=link_hash)"）。
    pub target: Option<String>,
    /// abort 之前的 state：`pending` / `running`（aborted=true 时）；或
    /// `completed`/`failed`/`aborted`（aborted=false 时）；job 不存在时为
    /// `None`。
    pub previous_state: Option<String>,
}
