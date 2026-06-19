//! ai_run flow 的输入 / 输出 DTO。

use rss_ai_news_domain::Score0To100;
use rss_ai_news_storage::AiCompleteArticleAdvance;

#[derive(Debug, Clone)]
pub struct AiRunOptions {
    /// task_gen 阶段一次扫描多少条 persisted article。
    pub task_gen_batch_size: u32,
    /// process 阶段一次 claim 多少条 pending AI 任务。
    pub process_batch_size: u32,
    pub max_attempts: u32,
    pub prompt_template: String,
    pub model_id: String,
    /// W14-A 失败回退链（已由 config effective 层 trim / 去重 / 去主模型）。process
    /// 阶段主模型锚定 `claimed.model_id`（行身份），链 = `[主模型, ...fallback_models]`。
    /// 空 = 不回退。task_gen 不使用此字段（只用主模型建 pending 行）。
    pub fallback_models: Vec<String>,
    pub max_input_chars: u32,
    pub max_tokens: u32,
    pub temperature: f32,
    /// 0..=100 的发布门槛。type-level invariant 在反序列化 / CLI 解析时已被
    /// `Score0To100` 锁死（F5-4），ai-run 路径同样使用 newtype 而不在中途
    /// 退化为 `i32`，与 publish / config 两侧保持类型契约一致（F6-1）。
    /// 调用 storage 层时按需 `.get() as i32`（SQL 绑定边界）。
    pub min_importance_score: Score0To100,
    /// 单次 run 内部 claim 循环上限。`0` = 不限。由 CLI 从
    /// `app.runtime.max_batches_per_run` 传入（F6-3）。仅约束 process 阶段
    /// 的 claim 循环；task_gen 阶段是 one-shot insert-pending sweep，不受
    /// 此上限控制。详见 docs/design/config-schema.md §4.4。
    pub max_batches: u32,
    pub category_key: String,
    /// CLI 调用前通过 `rule_version_repo.get_or_create("prompt", version_tag, ...)`
    /// 在 `rule_versions` 表中找到或插入对应 `(kind, version_tag)` 行后得到的
    /// `rule_versions.id`，写入 `article_ai_results.prompt_version`（详见
    /// storage-schema §4.6 幂等四元组）。注意这是按 tag 解析得到的 id，不是
    /// "active prompt"（本仓库无 active resolver 语义）。
    pub prompt_version: i64,
    /// CLI 调用前通过 `rule_version_repo.get_or_create("ai_output_schema",
    /// version_tag, ...)` 在 `rule_versions` 表中找到或插入对应 `(kind,
    /// version_tag)` 行后得到的 `rule_versions.id`，写入
    /// `article_ai_results.output_schema_version`。
    pub output_schema_version: i64,
}

#[derive(Debug, Default, Clone)]
pub struct AiRunSummary {
    pub task_gen: TaskGenSummary,
    pub process: AiProcessSummary,
}

#[derive(Debug, Default, Clone)]
pub struct TaskGenSummary {
    pub scanned: u32,
    pub inserted: u32,
    pub conflict_skipped: u32,
    pub article_already_advanced: u32,
}

#[derive(Debug, Default, Clone)]
pub struct AiProcessSummary {
    pub claimed: u32,
    pub succeeded: u32,
    pub filtered: u32,
    pub retryable_failed: u32,
    pub permanent_failed: u32,
    /// 因 task panic / cancel 而失败的 AI 任务数（codex P2-1）。这类失败不进入
    /// `per_task`，故 `recalculate_process_summary` 不重算它——与 `permanent_failed`
    /// （进入 per_task 的业务永久失败）分开计，避免被 recalc 清零而隐身。
    pub tasks_panicked: u32,
    /// 实际执行的批次数（F6-3）。命中 `max_batches` 时等于上限；否则小于上限。
    pub batches_executed: u32,
    /// `true` 表示循环因 `max_batches` 上限退出（仍有 pending 任务）；
    /// `false` 表示自然耗尽（claim 返回空批次）或因 retryable 失败 defer。
    /// 与 `retryable_deferred` 互斥（同一 run 内最多一个为 `true`）。
    pub max_batches_reached: bool,
    /// `true` 表示循环因本批次出现 RetryableFailed 主动 defer 到下次 run
    /// （F6-3 retryable-bail 路径；W4-1）。三值组合 `(max_batches_reached,
    /// retryable_deferred)` = `(T, F)` / `(F, T)` / `(F, F)` 区分 cap-hit /
    /// retryable-deferred / queue-exhausted 三种退出路径。
    pub retryable_deferred: bool,
    pub per_task: Vec<AiTaskOutcome>,
}

#[derive(Debug, Clone)]
pub struct AiTaskOutcome {
    pub article_ai_result_id: i64,
    pub article_id: i64,
    pub status: AiTaskStatus,
    pub article_advance: Option<AiCompleteArticleAdvance>,
    pub error_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiTaskStatus {
    Succeeded,
    Filtered,
    RetryableFailed,
    PermanentFailed,
}
