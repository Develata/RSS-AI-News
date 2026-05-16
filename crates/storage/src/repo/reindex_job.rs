//! reindex_job 持久化层（F15-5 W9-F3）。
//!
//! 物理表：见 [storage-schema §4.10 `reindex_jobs`](../../../../docs/design/storage-schema.md#410-reindex_jobs)。
//! 状态轮：见 [state-machine §6](../../../../docs/design/state-machine.md#6-reindex_job-独立状态轮)。
//!
//! 本模块提供 reindex_job 的原子原语：
//!   - `start_reindex_tx` —— **单事务**两 INSERT：rule_versions(status='pending') +
//!     reindex_jobs(state='pending')。失败整段回滚，避免 rule_versions 留
//!     "孤儿 pending" 行（F15-7 W9-F4）
//!   - `finish_reindex_tx` —— **单事务**跨表 finalize：reindex_jobs `running` →
//!     `completed` + 旧 active 行 → `superseded` + pending 行 → `active`。先
//!     demote 后 promote 避开 partial unique `uq_rule_versions_kind_active`；
//!     lease guard 失败整段回滚（F15-9 W9-F4）
//!   - `insert_pending` —— (无) → `pending`（partial unique 拒绝同 target 重复未完成 job）
//!   - `claim_pending` —— `pending` → `running`（写 lease + started_at + attempt_count += 1）
//!   - `advance_checkpoint` —— `running` → `running`（写 last_processed_id）
//!   - `advance_to_completed` —— `running` → `completed`（**仅**改 reindex_jobs；F15-9
//!     finish TX 接入后留作中间原语 / 单测 fixture，生产路径走 `finish_reindex_tx`）
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

/// [`ReindexJobRepository::start_reindex_tx`] 返回值：跨表 TX 同时新建的
/// `rule_versions` 与 `reindex_jobs` 行 id。F15-9 finish TX 用 `job_id`
/// 寻址 reindex_jobs 行，用 `rule_version_id` 推进 rule_versions 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartReindexTxOutcome {
    pub rule_version_id: i64,
    pub job_id: i64,
}

/// [`ReindexJobRepository::finish_reindex_tx`] 返回值。
///
/// - `job_completed = false` 表示 reindex_jobs 行的 lease guard 失败
///   （`state != 'running'` 或 `lease_owner` 不匹配，通常因 lease 被 reclaim）；
///   此时整段事务回滚，rule_versions 状态 **不会**被推进。调用方应当根据
///   该字段决定是否 warn 并放弃后续步骤——不要把这视为错误。
/// - `demoted_rule_version_id` 是被推到 `superseded` 的旧 active 行 id；
///   首次 reindex（该 kind 下尚无 active 行）时为 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinishReindexTxOutcome {
    pub job_completed: bool,
    pub demoted_rule_version_id: Option<i64>,
}

#[async_trait]
pub trait ReindexJobRepository: Send + Sync {
    /// reindex 启动入口（F15-7 W9-F4）。**单事务**串联两条 INSERT：
    ///
    /// 1. `rule_versions(kind, version_tag, description, payload_sha256,
    ///    status='pending')` → `rule_version_id`
    /// 2. `reindex_jobs(target, rule_version_id, state='pending',
    ///    attempt_count=0)` → `job_id`
    ///
    /// 任何一步失败整段回滚：
    ///   - rule_versions UNIQUE `(kind, version_tag)` 冲突 → 回滚，无新 job
    ///   - reindex_jobs partial unique `uq_reindex_jobs_target_active` 冲突
    ///     （同 target 已有 pending/running job）→ 回滚，rule_versions 行
    ///     也不写入（避免"孤儿 pending"）
    ///
    /// 两条 INSERT 的 UNIQUE 违例都通过 [`classify_sqlite_error`] 映射为
    /// [`StorageError::Conflict { table, key }`]，调用方靠 `table` 字段
    /// 区分是 rule_versions tag 重复还是 target 已被占用。
    async fn start_reindex_tx(
        &self,
        rule_kind: &str,
        rule_version_tag: &str,
        rule_description: &str,
        rule_payload_sha256: &str,
        target: &str,
        now: OffsetDateTime,
    ) -> Result<StartReindexTxOutcome, StorageError>;

    /// **F15-7 过渡原语**：在 F15-8（claim + lease + checkpoint）与 F15-9
    /// （跨表 finish TX）接入前，允许 reindex flow 在 INSERT 完成后直接把
    /// 当前 job 推到 `completed`，避免下一次同 target reindex 被 partial
    /// unique 拒绝。**不**校验 `lease_owner`，仅要求 `state='pending'`。
    ///
    /// 语义边界：
    ///   - 仅给 reindex flow 内部使用，不应出现在 worker/lease 路径
    ///   - F15-9 把 finish TX 接入后，该方法保留作单测 fixture / 中间原语，
    ///     生产路径改走 [`Self::finish_reindex_tx`]
    async fn complete_without_claim(
        &self,
        id: i64,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError>;

    /// reindex 终止入口（F15-9 W9-F4）。**单事务**串联三步 UPDATE：
    ///
    /// 1. `reindex_jobs`: `running` → `completed`，带 lease guard
    ///    （`state='running' AND lease_owner=:owner`）。`rows_affected==0`
    ///    （lease 被 reclaim 或外部状态变更）→ 整段回滚，返回
    ///    [`FinishReindexTxOutcome::job_completed`] = `false`，rule_versions
    ///    **保持原状**。
    /// 2. `rule_versions`: 把 `kind=:rule_kind AND status='active' AND id != :rule_version_id`
    ///    的行 demote 到 `'superseded'` 并写 `retired_at`。最多一行
    ///    （partial unique 保证）；首次 reindex 时 0 行，记入 outcome 的
    ///    `demoted_rule_version_id = None`。
    /// 3. `rule_versions`: 把 `id=:rule_version_id AND kind=:rule_kind AND
    ///    status='pending'` 的行 promote 到 `'active'`。`rows_affected != 1`
    ///    视为协议违例（rule_version_id 不是该 kind 的 pending 行）→ 整段
    ///    回滚并返回 [`StorageError`]。
    ///
    /// **顺序**：先 demote 后 promote。partial unique
    /// `uq_rule_versions_kind_active`(kind WHERE status='active') 在每条
    /// statement 后立即检查；反向顺序会在 promote 时与旧 active 行冲突。
    ///
    /// 该方法不修改 `lease_owner` / `lease_expires_at` 之外的 reindex_jobs
    /// 字段（与 [`Self::advance_to_completed`] 行为一致），把 lease 清空
    /// 并写 `finished_at`。
    async fn finish_reindex_tx(
        &self,
        job_id: i64,
        owner: &str,
        rule_version_id: i64,
        rule_kind: &str,
        finished_at: OffsetDateTime,
    ) -> Result<FinishReindexTxOutcome, StorageError>;

    /// `(无) → pending`。`partial unique` 索引 `uq_reindex_jobs_target_active`
    /// 保证同 target 同时只能有一个 `pending`/`running` job；冲突返回
    /// [`StorageError::Conflict`]（classify_sqlite_error 把 UNIQUE 违例映射为
    /// Conflict）。
    ///
    /// 注：reindex flow 入口应使用 [`Self::start_reindex_tx`] 把 rule_versions
    /// 与 reindex_jobs 两条 INSERT 包到同事务；本方法仅在已有 rule_version_id
    /// 的脚本/迁移/测试场景下使用。
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

    /// `pending → running`，但**按 id 寻址**。reindex flow 在
    /// [`Self::start_reindex_tx`] 之后已经持有 `job_id`，应该直接 claim 自己
    /// 刚创建的 job，而非走 [`Self::claim_pending`] 的 `(created_at ASC,
    /// id ASC)` 扫描（后者在并发或残留 pending 的情况下会拿错 job）。
    ///
    /// 与 `claim_pending` 共享 lease 语义：
    ///   - `started_at = COALESCE(started_at, :now)` 保留首次 claim 时间
    ///   - `attempt_count += 1`
    ///   - 仅在 `state='pending'` **且** (`lease_expires_at IS NULL` 或 已过期)
    ///     时成功；否则返 `None`（job 已 running 或 lease 仍有效，**不**报错）
    async fn claim_by_id(
        &self,
        id: i64,
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

    /// 仅校验 lease 是否仍由 `owner` 持有；不写 last_processed_id。
    /// reindex flow 中 `categories` 这种"无 after_id 分页 / 无 checkpoint
    /// 意义"的 target 用此原语在每次数据写之间插入 guard。返回
    /// `Ok(true)` ↔ `(state='running' AND lease_owner=owner)` 行存在。
    ///
    /// 实现内部把 `updated_at = now` 顺手刷新，让 reclaim 巡检通过 `updated_at`
    /// 推断"worker 仍在活动"。F15-fix2 加入。
    async fn assert_lease_held(
        &self,
        id: i64,
        owner: &str,
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
    async fn start_reindex_tx(
        &self,
        rule_kind: &str,
        rule_version_tag: &str,
        rule_description: &str,
        rule_payload_sha256: &str,
        target: &str,
        now: OffsetDateTime,
    ) -> Result<StartReindexTxOutcome, StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::from)?;

        let rule_version_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status)
            VALUES ($1, $2, $3, $4, 'pending')
            RETURNING id
            "#,
        )
        .bind(rule_kind)
        .bind(rule_version_tag)
        .bind(rule_description)
        .bind(rule_payload_sha256)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "rule_versions",
                format!("{rule_kind}/{rule_version_tag}"),
            )
        })?;

        let job_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO reindex_jobs (
                target, rule_version_id, state, attempt_count,
                created_at, updated_at
            )
            VALUES ($1, $2, 'pending', 0, $3, $4)
            RETURNING id
            "#,
        )
        .bind(target)
        .bind(rule_version_id)
        .bind(now)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "reindex_jobs",
                format!("target={target}/rule_version_id={rule_version_id}"),
            )
        })?;

        tx.commit().await.map_err(StorageError::from)?;
        Ok(StartReindexTxOutcome {
            rule_version_id,
            job_id,
        })
    }

    async fn complete_without_claim(
        &self,
        id: i64,
        finished_at: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET state = 'completed',
                lease_owner = NULL,
                lease_expires_at = NULL,
                finished_at = $1,
                updated_at = $2
            WHERE id = $3 AND state = 'pending'
            "#,
        )
        .bind(finished_at)
        .bind(finished_at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(StorageError::from)?;
        Ok(result.rows_affected() == 1)
    }

    async fn finish_reindex_tx(
        &self,
        job_id: i64,
        owner: &str,
        rule_version_id: i64,
        rule_kind: &str,
        finished_at: OffsetDateTime,
    ) -> Result<FinishReindexTxOutcome, StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::from)?;

        // 1) reindex_jobs running → completed（带 lease guard）。
        let job_update = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET state = 'completed',
                lease_owner = NULL,
                lease_expires_at = NULL,
                finished_at = $1,
                updated_at = $2
            WHERE id = $3 AND state = 'running' AND lease_owner = $4
            "#,
        )
        .bind(finished_at)
        .bind(finished_at)
        .bind(job_id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        if job_update.rows_affected() == 0 {
            // lease guard 失败：放弃整段事务，rule_versions 保持原状。
            tx.rollback().await.map_err(StorageError::from)?;
            return Ok(FinishReindexTxOutcome {
                job_completed: false,
                demoted_rule_version_id: None,
            });
        }

        // 2) 旧 active demote 到 superseded。partial unique 保证最多一行；
        //    首次 reindex 时无旧 active，返回 None。
        let demoted_rule_version_id = sqlx::query_scalar::<_, i64>(
            r#"
            UPDATE rule_versions
            SET status = 'superseded',
                retired_at = $1
            WHERE kind = $2 AND status = 'active' AND id != $3
            RETURNING id
            "#,
        )
        .bind(finished_at)
        .bind(rule_kind)
        .bind(rule_version_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;

        // 3) pending → active（partial unique 此刻已无 active 行）。
        //    rows_affected != 1 视为协议违例：rule_version_id 不是该 kind
        //    的 pending 行（可能被外部状态破坏），整段回滚并报错。
        let promote = sqlx::query(
            r#"
            UPDATE rule_versions
            SET status = 'active'
            WHERE id = $1 AND kind = $2 AND status = 'pending'
            "#,
        )
        .bind(rule_version_id)
        .bind(rule_kind)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        if promote.rows_affected() != 1 {
            tx.rollback().await.map_err(StorageError::from)?;
            return Err(StorageError::Conflict {
                table: "rule_versions".to_string(),
                key: format!("id={rule_version_id}/kind={rule_kind} 非 pending 状态"),
            });
        }

        tx.commit().await.map_err(StorageError::from)?;
        Ok(FinishReindexTxOutcome {
            job_completed: true,
            demoted_rule_version_id,
        })
    }

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
            VALUES ($1, $2, 'pending', 0, $3, $4)
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
                lease_owner = $1,
                lease_expires_at = $2,
                started_at = COALESCE(started_at, $3),
                attempt_count = attempt_count + 1,
                updated_at = $4
            WHERE id = (
                SELECT id FROM reindex_jobs
                WHERE state = 'pending'
                  AND (lease_expires_at IS NULL OR lease_expires_at < $5)
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

    async fn claim_by_id(
        &self,
        id: i64,
        owner: &str,
        now: OffsetDateTime,
        lease_expires_at: OffsetDateTime,
    ) -> Result<Option<ClaimedReindexJob>, StorageError> {
        sqlx::query_as::<_, ClaimedReindexJob>(
            r#"
            UPDATE reindex_jobs
            SET state = 'running',
                lease_owner = $1,
                lease_expires_at = $2,
                started_at = COALESCE(started_at, $3),
                attempt_count = attempt_count + 1,
                updated_at = $4
            WHERE id = $5
              AND state = 'pending'
              AND (lease_expires_at IS NULL OR lease_expires_at < $6)
            RETURNING id, target, rule_version_id, last_processed_id, attempt_count
            "#,
        )
        .bind(owner)
        .bind(lease_expires_at)
        .bind(now)
        .bind(now)
        .bind(id)
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
            SET last_processed_id = $1, updated_at = $2
            WHERE id = $3 AND state = 'running' AND lease_owner = $4
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

    async fn assert_lease_held(
        &self,
        id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let result = sqlx::query(
            r#"
            UPDATE reindex_jobs
            SET updated_at = $1
            WHERE id = $2 AND state = 'running' AND lease_owner = $3
            "#,
        )
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
                finished_at = $1,
                updated_at = $2
            WHERE id = $3 AND state = 'running' AND lease_owner = $4
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
                error = $1,
                lease_owner = NULL,
                lease_expires_at = NULL,
                finished_at = $2,
                updated_at = $3
            WHERE id = $4 AND state = 'running' AND lease_owner = $5
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
                aborted_reason = $1,
                lease_owner = NULL,
                lease_expires_at = NULL,
                finished_at = $2,
                updated_at = $3
            WHERE id = $4 AND state IN ('pending', 'running')
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
                updated_at = $1
            WHERE state = 'running'
              AND lease_expires_at IS NOT NULL
              AND lease_expires_at < $2
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
            format!("SELECT {SELECT_REINDEX_JOB_COLUMNS} FROM reindex_jobs WHERE id = $1 LIMIT 1");
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
             WHERE target = $1 AND state IN ('pending', 'running') \
             LIMIT 1"
        );
        sqlx::query_as::<_, ReindexJob>(&sql)
            .bind(target)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::from)
    }
}
