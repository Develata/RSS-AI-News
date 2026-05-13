//! reindex_job 持久化层（F15-5 W9-F3）。
//!
//! 物理表：见 [storage-schema §4.10 `reindex_jobs`](../../../../docs/design/storage-schema.md#410-reindex_jobs)。
//! 状态轮：见 [state-machine §6](../../../../docs/design/state-machine.md#6-reindex_job-独立状态轮)。
//!
//! 本模块提供 reindex_job 的原子原语：
//!   - `insert_pending` —— (无) → `pending`（partial unique 拒绝同 target 重复未完成 job）
//!   - `claim_pending` —— `pending` → `running`（写 lease + started_at + attempt_count += 1）
//!   - `advance_checkpoint` —— `running` → `running`（写 last_processed_id）
//!   - `advance_to_completed` —— `running` → `completed`（**仅**改 reindex_jobs；rule_versions
//!     `pending → active` + 旧 active → `superseded` 的跨表事务由 F15-9 reindex finish
//!     flow 内组合，避免把跨 repo TX 耦合到 storage 层）
//!   - `mark_failed` —— `running` → `failed`
//!   - `abort` —— `pending` / `running` → `aborted`（用户主动）
//!   - `reclaim_expired_leases` —— `running` → `pending`（lease 过期，清 lease，保留
//!     `last_processed_id` / `started_at`；与 state-machine §2.3 lease 总则一致）
//!   - `find_by_id` / `find_active_by_target` / `list_running` —— 读路径

use async_trait::async_trait;
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, classify_sqlite_error};

#[derive(Debug, Clone, FromRow)]
pub struct ReindexJob {
    pub id: i64,
    pub target: String,
    pub rule_version_id: i64,
    pub last_processed_id: Option<i64>,
    pub total_estimated: Option<i64>,
    pub state: String,
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

/// `claim_pending` 返回值：只暴露 reindex flow 在批处理循环中需要的字段，
/// 屏蔽 lease/timestamps 细节。
#[derive(Debug, Clone, FromRow)]
pub struct ClaimedReindexJob {
    pub id: i64,
    pub target: String,
    pub rule_version_id: i64,
    pub last_processed_id: Option<i64>,
    pub attempt_count: i64,
}

#[async_trait]
pub trait ReindexJobRepository: Send + Sync {
    /// `(无) → pending`。`partial unique` 索引 `uq_reindex_jobs_target_active`
    /// 保证同 target 同时只能有一个 `pending`/`running` job；冲突返回
    /// [`StorageError::Conflict`]（classify_sqlite_error 把 UNIQUE 违例映射为
    /// Conflict）。
    async fn insert_pending(
        &self,
        target: &str,
        rule_version_id: i64,
        now: OffsetDateTime,
    ) -> Result<i64, StorageError>;

    /// `pending → running`。一次只 claim 一个 job（reindex 不像 ingest 那样
    /// 批量并发，单 job 内部按 `batch_size` 自循环），所以返回 `Option`
    /// 而非 `Vec`。`started_at = COALESCE(started_at, :now)` 保留首次 claim
    /// 时间；reclaim 后再次 claim 不重置 started_at。
    async fn claim_pending(
        &self,
        owner: &str,
        now: OffsetDateTime,
        lease_expires_at: OffsetDateTime,
    ) -> Result<Option<ClaimedReindexJob>, StorageError>;

    /// `running → running`（checkpoint 提交）。`WHERE state='running' AND
    /// lease_owner = :owner` 双 guard 防止 lease 被 reclaim 后原 worker 还
    /// 在写。返回是否真的更新了一行。
    async fn advance_checkpoint(
        &self,
        id: i64,
        owner: &str,
        last_processed_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;

    /// `running → completed`。**仅**更新 reindex_jobs；跨表激活（rule_versions
    /// `pending → active`）由 F15-9 reindex finish flow 用 sqlx transaction
    /// 与本方法和 RuleVersionRepository 的对应方法组合调用。本方法把
    /// reindex_jobs 这一边做成幂等原语。
    async fn advance_to_completed(
        &self,
        id: i64,
        owner: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError>;

    /// `running → failed`。写 `error` + `finished_at`；rule_versions 行保持
    /// `pending` 由管理员决定是否清理。
    async fn mark_failed(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError>;

    /// `pending` / `running` → `aborted`。用户主动；写 `aborted_reason` +
    /// `finished_at`；不要求持有 lease（pending 状态不持 lease）。终态不允许
    /// 再 abort。
    async fn abort(
        &self,
        id: i64,
        aborted_reason: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError>;

    /// `running → pending`（lease 过期 reclaim）。清 `lease_owner` /
    /// `lease_expires_at`，**保留** `last_processed_id` 与 `started_at`，
    /// **不**改 `attempt_count`。返回被 reclaim 的行数。
    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError>;

    /// `migrate` 阻塞门：返回所有 `state IN ('pending','running')` 的 job，
    /// 由调用方判断是否非空决定是否拒绝 migrate（F15-11）。
    async fn list_running(&self) -> Result<Vec<ReindexJob>, StorageError>;

    async fn find_by_id(&self, id: i64) -> Result<Option<ReindexJob>, StorageError>;

    /// 按 target 查 `pending`/`running` 的活动 job（最多一行；partial unique
    /// 保证）。用于 `cli reindex --abort --target X` 之类按 target 寻址。
    async fn find_active_by_target(&self, target: &str)
    -> Result<Option<ReindexJob>, StorageError>;
}

#[derive(Debug, Clone)]
pub struct SqliteReindexJobRepo {
    pool: SqlitePool,
}

impl SqliteReindexJobRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const SELECT_REINDEX_JOB_COLUMNS: &str = r#"
    id, target, rule_version_id, last_processed_id, total_estimated,
    state, error, aborted_reason, lease_owner, lease_expires_at,
    attempt_count, started_at, finished_at, created_at, updated_at
"#;

#[async_trait]
impl ReindexJobRepository for SqliteReindexJobRepo {
    async fn insert_pending(
        &self,
        target: &str,
        rule_version_id: i64,
        now: OffsetDateTime,
    ) -> Result<i64, StorageError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO reindex_jobs (
                target, rule_version_id, state, attempt_count,
                created_at, updated_at
            )
            VALUES (?, ?, 'pending', 0, ?, ?)
            RETURNING id
            "#,
        )
        .bind(target)
        .bind(rule_version_id)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "reindex_jobs",
                format!("target={target}/rule_version_id={rule_version_id}"),
            )
        })
    }

    async fn claim_pending(
        &self,
        owner: &str,
        now: OffsetDateTime,
        lease_expires_at: OffsetDateTime,
    ) -> Result<Option<ClaimedReindexJob>, StorageError> {
        sqlx::query_as::<_, ClaimedReindexJob>(
            r#"
            UPDATE reindex_jobs
            SET state = 'running',
                lease_owner = ?,
                lease_expires_at = ?,
                started_at = COALESCE(started_at, ?),
                attempt_count = attempt_count + 1,
                updated_at = ?
            WHERE id = (
                SELECT id FROM reindex_jobs
                WHERE state = 'pending'
                  AND (lease_expires_at IS NULL OR lease_expires_at < ?)
                ORDER BY created_at ASC, id ASC
                LIMIT 1
            )
            RETURNING id, target, rule_version_id, last_processed_id, attempt_count
            "#,
        )
        .bind(owner)
        .bind(lease_expires_at)
        .bind(now)
        .bind(now)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::from)
    }

    async fn advance_checkpoint(
        &self,
        id: i64,
        owner: &str,
        last_processed_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET last_processed_id = ?, updated_at = ?
            WHERE id = ? AND state = 'running' AND lease_owner = ?
            "#,
        )
        .bind(last_processed_id)
        .bind(now)
        .bind(id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn advance_to_completed(
        &self,
        id: i64,
        owner: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET state = 'completed',
                lease_owner = NULL,
                lease_expires_at = NULL,
                finished_at = ?,
                updated_at = ?
            WHERE id = ? AND state = 'running' AND lease_owner = ?
            "#,
        )
        .bind(finished_at)
        .bind(finished_at)
        .bind(id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_failed(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET state = 'failed',
                error = ?,
                lease_owner = NULL,
                lease_expires_at = NULL,
                finished_at = ?,
                updated_at = ?
            WHERE id = ? AND state = 'running' AND lease_owner = ?
            "#,
        )
        .bind(error)
        .bind(finished_at)
        .bind(finished_at)
        .bind(id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn abort(
        &self,
        id: i64,
        aborted_reason: &str,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET state = 'aborted',
                aborted_reason = ?,
                lease_owner = NULL,
                lease_expires_at = NULL,
                finished_at = ?,
                updated_at = ?
            WHERE id = ? AND state IN ('pending', 'running')
            "#,
        )
        .bind(aborted_reason)
        .bind(finished_at)
        .bind(finished_at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET state = 'pending',
                lease_owner = NULL,
                lease_expires_at = NULL,
                updated_at = ?
            WHERE state = 'running'
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at < ?
            "#,
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected())
    }

    async fn list_running(&self) -> Result<Vec<ReindexJob>, StorageError> {
        let sql = format!(
            "SELECT {SELECT_REINDEX_JOB_COLUMNS} \
             FROM reindex_jobs \
             WHERE state IN ('pending', 'running') \
             ORDER BY id ASC"
        );
        sqlx::query_as::<_, ReindexJob>(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(StorageError::from)
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<ReindexJob>, StorageError> {
        let sql =
            format!("SELECT {SELECT_REINDEX_JOB_COLUMNS} FROM reindex_jobs WHERE id = ? LIMIT 1");
        sqlx::query_as::<_, ReindexJob>(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::from)
    }

    async fn find_active_by_target(
        &self,
        target: &str,
    ) -> Result<Option<ReindexJob>, StorageError> {
        let sql = format!(
            "SELECT {SELECT_REINDEX_JOB_COLUMNS} \
             FROM reindex_jobs \
             WHERE target = ? AND state IN ('pending', 'running') \
             LIMIT 1"
        );
        sqlx::query_as::<_, ReindexJob>(&sql)
            .bind(target)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::from)
    }
}
