//! article_ai_results 持久化层（契约）。
//!
//! ## W11-P3-E-3：PG 分支落地
//!
//! 按 `docs-backup/design/storage-multi-dialect.md` §6.2 模式：trait method `match`
//! 分发到 sqlite_*/pg_* 私有 helper + 共享 SQL const + new_with_storage 入口。
//! SQL const 见 `super::article_ai_result_sql`，方言分发实装见
//! `super::article_ai_result_impl`。
//!
//! **§6.4 PG 契约**：`claim_pending` 子查询必须 `FOR UPDATE OF aar SKIP LOCKED`（只锁 AI 行），
//! 让 ai-run 多 worker 并发 claim 同一 pending 池时各自拿到不同候选。
//! SQL 因此分裂为 `CLAIM_AI_PENDING_SQLITE_SQL` / `CLAIM_AI_PENDING_PG_SQL`。
//!
//! 跨表事务（`insert_pending_and_advance_article` /
//! `release_success_and_advance_article`）在 PG 上语义等价：READ COMMITTED +
//! row-level lock 保证 lease guard 失败 → 整段回滚（与 SQLite WAL 整库
//! 写锁等价，详见设计 §2.3 表格）。

use async_trait::async_trait;
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{ClaimRequest, ReleaseFailureOutcome, StorageError, StoragePool};

#[derive(Debug, Clone)]
pub struct NewAiResult {
    pub article_id: i64,
    pub prompt_version: i64,
    pub output_schema_version: i64,
    pub model_id: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClaimedAiResult {
    pub id: i64,
    pub article_id: i64,
    pub prompt_version: i64,
    pub output_schema_version: i64,
    pub model_id: String,
}

#[derive(Debug, Clone)]
pub struct AiSuccessOutcome {
    pub summary: String,
    pub tags_json: String,
    pub importance_score: Option<i32>,
    pub keep_decision: Option<bool>,
    pub raw_response_artifact_id: Option<i64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cost_micro_usd: Option<i64>,
    pub latency_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct InsertPendingOutcome {
    pub ai_result_id: Option<i64>,
    /// The article left `persisted` in this call (insert or reconcile).
    pub article_advanced: bool,
    pub article_already_advanced: bool,
    /// The identity row already existed and the still-`persisted` article was
    /// moved to the state that row implies (C3): `pending` / `running` →
    /// `ai_pending`; `succeeded` / `filtered` → the post-AI state a normal
    /// success release produces.
    pub reconciled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiCompleteArticleAdvance {
    /// keep_decision=1 且 score >= min_importance_score → ready_for_publish
    ReadyForPublish,
    /// keep_decision=1 且 score < min_importance_score → ai_done
    AiDone,
    /// keep_decision=0 且不存在其他 succeeded 行 → publish_skipped
    PublishSkipped,
    /// 不更新 articles.state。
    NoChange,
}

#[derive(Debug, Clone)]
pub struct ReleaseSuccessOutcome {
    pub released: bool,
    pub article_advance: AiCompleteArticleAdvance,
}

#[async_trait]
pub trait ArticleAiResultRepository: Send + Sync {
    async fn insert_pending(&self, item: &NewAiResult) -> Result<Option<i64>, StorageError>;
    async fn claim_pending(
        &self,
        request: &ClaimRequest,
        category_key: &str,
    ) -> Result<Vec<ClaimedAiResult>, StorageError>;
    /// C9：同 [`Self::claim_pending`]，但只领取 publish 窗口内（"fresh"）文章的
    /// pending 任务：`COALESCE(fe.published_at, fe.discovered_at) >=
    /// published_since`，按该来源时间升序（最先掉出窗口的先领）、任务 id 次序。
    async fn claim_pending_fresh(
        &self,
        request: &ClaimRequest,
        category_key: &str,
        published_since: OffsetDateTime,
    ) -> Result<Vec<ClaimedAiResult>, StorageError>;

    /// C9 batch=1 轮换信号（task_gen）：该分类最新（id 最大）的 AI 任务是否属于
    /// 窗口内文章；分类尚无任务 → `None`。
    async fn newest_task_is_fresh(
        &self,
        category_key: &str,
        published_since: OffsetDateTime,
    ) -> Result<Option<bool>, StorageError>;

    /// C9 batch=1 轮换信号（claim）：该分类最近一次首次领取（`started_at` 最大）
    /// 的任务是否属于窗口内文章；从未领取 → `None`。
    async fn last_claim_is_fresh(
        &self,
        category_key: &str,
        published_since: OffsetDateTime,
    ) -> Result<Option<bool>, StorageError>;

    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        outcome: AiSuccessOutcome,
        effective_model_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    /// W15 §3 折叠：retryable 失败按 `attempt_count >= max_attempts` 在 SQL 内
    /// 决定回 `pending` / 转 `permanent_failed`。`last_error*` 写真实底层错误。
    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError>;
    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;
    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError>;

    /// W15 §4 sweep：`pending` + `attempt_count >= max_attempts` + lease 空/过期
    /// → `permanent_failed`。兜底 release 折叠摸不到的行（设计落地前的遗留卡死行、
    /// 崩溃在最后一次尝试经 reclaim 回 pending 的行）。保留既有 `last_error*`。
    /// 返回转终态的行数。
    async fn terminalize_exhausted(
        &self,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError>;

    /// task_gen：同事务 INSERT article_ai_results state='pending' + UPDATE articles
    /// `persisted → ai_pending`（article 已不是 persisted 时回滚插入）。
    ///
    /// 身份行已存在（唯一键冲突）且 article 仍为 `persisted` 时，同事务按该行状态
    /// 对账 article：`pending` / `running` → `ai_pending`；`succeeded` / `filtered`
    /// → 与正常成功 release 相同的派生态（`min_importance_score` 定门槛）；
    /// `permanent_failed` → 保持 `persisted`。PG 对已有行加 `FOR UPDATE`，与并发
    /// success release 串行化。其它 article 状态（含 `published`）不动。
    async fn insert_pending_and_advance_article(
        &self,
        item: &NewAiResult,
        min_importance_score: i32,
        now: OffsetDateTime,
    ) -> Result<InsertPendingOutcome, StorageError>;

    /// backfill：同事务 INSERT pending 行（冲突跳过），并把仍为 `persisted` 的
    /// article 推进到 `ai_pending`——新插入，或冲突的已有行为 `pending` / `running`
    /// 时。已推进的 article（`ai_done`、`published` 等）保持不变，插入照常保留。
    async fn insert_backfill_pending(
        &self,
        item: &NewAiResult,
        now: OffsetDateTime,
    ) -> Result<InsertPendingOutcome, StorageError>;

    /// 同事务：release 成功 AI 结果，并按 AI 输出派生推进 articles.state。
    async fn release_success_and_advance_article(
        &self,
        id: i64,
        owner: &str,
        outcome: AiSuccessOutcome,
        effective_model_id: &str,
        article_id: i64,
        min_importance_score: i32,
        now: OffsetDateTime,
    ) -> Result<ReleaseSuccessOutcome, StorageError>;
}

#[derive(Debug, Clone)]
pub struct ArticleAiResultRepo {
    pub(super) pool: StoragePool,
}

impl ArticleAiResultRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    /// W11-P3-E-3：PG 入口；旧 `new(SqlitePool)` thin wrapper 保留兼容。
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }
}

impl AiCompleteArticleAdvance {
    pub(super) fn as_article_state_str(&self) -> Option<&'static str> {
        match self {
            Self::ReadyForPublish => Some("ready_for_publish"),
            Self::AiDone => Some("ai_done"),
            Self::PublishSkipped => Some("publish_skipped"),
            Self::NoChange => None,
        }
    }
}
