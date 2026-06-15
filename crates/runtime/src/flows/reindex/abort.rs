//! `reindex --abort <job_id>`：把 pending/running job 推到 aborted。

use serde_json::json;
use time::OffsetDateTime;

use crate::error::RuntimeError;
use crate::events::RunEventEmitter;

use super::{ReindexAbortOutcome, ReindexFlow};

impl ReindexFlow {
    /// 取消指定 `reindex_jobs.id`：把 `pending`/`running` 推到 `aborted`，
    /// 清 lease，写 `aborted_reason` + `finished_at`。cli-semantics §4.8
    /// line 290。
    ///
    /// 设计要点：
    ///   - **保留已更新批次**：abort 不回滚 advance_checkpoint 已落地的
    ///     last_processed_id，也不回滚 reindex 阶段已 UPDATE 的数据行；
    ///     active rule 仍是旧版（rule_versions pending 行保持 pending）
    ///     提供"读路径不受 reindex 影响"的语义保证
    ///   - **不持 lease 也可 abort**：abort 是用户主动操作，无需校验
    ///     lease_owner；与 reclaim_expired_leases 共存（lease 过期回到
    ///     pending 后仍可 abort）
    ///   - **幂等**：job 已 terminal 时返回 `aborted=false` 不算错误
    pub async fn abort(
        &self,
        job_id: i64,
        reason: &str,
    ) -> Result<ReindexAbortOutcome, RuntimeError> {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "reindex",
            repo: self.ctx.event_repo.as_ref(),
        };

        let previous = self.ctx.reindex_job_repo.find_by_id(job_id).await?;
        let Some(job) = previous else {
            emitter
                .emit(
                    "run_completed",
                    "warn",
                    Some("reindex_job"),
                    Some(job_id),
                    "reindex --abort: job not found",
                    Some(json!({ "reindex_job_id": job_id, "reason": reason })),
                )
                .await;
            return Ok(ReindexAbortOutcome {
                job_id,
                aborted: false,
                target: None,
                previous_state: None,
            });
        };

        let finished_at = OffsetDateTime::now_utc();
        let aborted = self
            .ctx
            .reindex_job_repo
            .abort(job_id, reason, finished_at)
            .await?;

        let (severity, message) = if aborted {
            ("info", "reindex --abort: job aborted")
        } else {
            ("warn", "reindex --abort: job already in terminal state")
        };
        emitter
            .emit(
                "run_completed",
                severity,
                Some("reindex_job"),
                Some(job_id),
                message,
                Some(json!({
                    "reindex_job_id": job_id,
                    "target": job.target,
                    "previous_state": job.state,
                    "reason": reason,
                })),
            )
            .await;

        Ok(ReindexAbortOutcome {
            job_id,
            aborted,
            target: Some(job.target),
            previous_state: Some(job.state),
        })
    }
}
