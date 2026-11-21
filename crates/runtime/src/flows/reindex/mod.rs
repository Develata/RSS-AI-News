//! reindex flow：rebuild link_hash / content_hash / feed_sources（categories）。
//!
//! 本文件持 lease 生命周期编排（start/claim/run_inner/finalize）+ checkpoint +
//! sha256 工具；abort 见 `abort`，dry-run 见 `dry_run`，真实写入循环见
//! `execute`，DTO 见 `dto`。

use std::sync::Arc;

use rss_ai_news_storage::{
    ClaimedReindexJob, ReindexBatchCheckpoint, ResumeReindexOutcome, StorageError, build_owner_id,
    lease_expires_at,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::context::ReindexDeps;
use crate::error::RuntimeError;
use crate::events::RunEventEmitter;
use crate::flows::maintenance::emit_maintenance_outcome;

mod abort;
mod dry_run;
mod dto;
mod execute;

pub use dto::*;
pub use rss_ai_news_domain::state::ReindexTarget;

pub struct ReindexFlow {
    ctx: Arc<ReindexDeps>,
}

impl ReindexFlow {
    pub fn new(ctx: Arc<ReindexDeps>) -> Self {
        Self { ctx }
    }

    pub async fn run(&self, opts: ReindexOptions) -> Result<ReindexSummary, RuntimeError> {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run.run_id,
            stage: "reindex",
            repo: self.ctx.event_repo.as_ref(),
        };
        emitter
            .emit(
                "run_started",
                "info",
                None,
                None,
                "reindex started",
                Some(json!({ "target": format!("{:?}", opts.target) })),
            )
            .await;

        // W15 §5：仅 ① reclaim（reindex claim 不过滤 attempt_count、失败
        // 直转终态，无预算耗尽语义，故无 ② sweep），best-effort。
        let maintenance_now = OffsetDateTime::now_utc();
        let reclaimed = self
            .ctx
            .reindex_job_repo
            .reclaim_expired_leases(maintenance_now)
            .await;
        emit_maintenance_outcome(&emitter, "reindex_jobs", reclaimed, None).await;

        // 05-storage §8.1 续跑：先在单事务内领取同 target、同规则（kind +
        // sha256）且无存活 lease 的活动 job，从其 checkpoint 继续；没有活动 job
        // 才走 F15-7 跨表 start TX（rule_versions(status='pending') +
        // reindex_jobs(state='pending') 同事务写入，任一冲突整段回滚）。活动 job
        // 规则不同或 lease 仍存活 → 显式冲突，提示 `reindex --abort <id>`，不改任何行。
        // lease 时长复用 `lease.ai_duration_seconds`（reindex 工作负载与 AI 接近：
        // 长批处理 + 大量 I/O），每批 checkpoint 续租。
        let target_str = opts.target.to_string();
        let owner = build_owner_id();
        let claim_now = OffsetDateTime::now_utc();
        let resume = self
            .ctx
            .reindex_job_repo
            .claim_resumable_by_target(
                &target_str,
                "reindex",
                &opts.new_rule_version_sha256,
                &owner,
                claim_now,
                self.lease_deadline(claim_now),
            )
            .await?;
        let claimed = match resume {
            ResumeReindexOutcome::Resumed(claimed) => {
                tracing::info!(
                    stage = "reindex",
                    target = %target_str,
                    job_id = claimed.id,
                    last_processed_id = ?claimed.last_processed_id,
                    "resuming reindex job from its checkpoint"
                );
                emitter
                    .emit(
                        "reindex_resumed",
                        "info",
                        Some("reindex_job"),
                        Some(claimed.id),
                        "reindex resumed from checkpoint",
                        Some(json!({
                            "target": target_str,
                            "last_processed_id": claimed.last_processed_id,
                            "attempt_count": claimed.attempt_count,
                        })),
                    )
                    .await;
                claimed
            }
            ResumeReindexOutcome::NoActiveJob => {
                self.start_new_job(&opts, &target_str, &owner).await?
            }
            ResumeReindexOutcome::RuleMismatch {
                job_id,
                state,
                version_tag,
                payload_sha256,
            } => {
                return Err(active_job_conflict(
                    &target_str,
                    job_id,
                    &format!(
                        "{state} job #{job_id} runs rule {version_tag} (sha256 {payload_sha256}), \
                         this run requests sha256 {}",
                        opts.new_rule_version_sha256
                    ),
                ));
            }
            ResumeReindexOutcome::Busy { job_id, state } => {
                return Err(active_job_conflict(
                    &target_str,
                    job_id,
                    &format!("{state} job #{job_id} is held by another worker with a live lease"),
                ));
            }
        };
        let rule_id = claimed.rule_version_id;
        let job_id = claimed.id;

        let mut summary = ReindexSummary {
            new_rule_version_id: rule_id,
            reindex_job_id: job_id,
            ..ReindexSummary::default()
        };

        let after_id_start = claimed.last_processed_id.unwrap_or(0);
        let inner_result = self
            .run_inner(&opts, job_id, &owner, after_id_start, &mut summary)
            .await;

        // F15-9 W9-F4 finalize：成功 → finish_reindex_tx（跨表事务，单事务内
        // 推进 reindex_jobs running → completed + 旧 active rule_versions →
        // superseded + pending rule_versions → active）；失败 → mark_failed
        // （reindex_jobs 单表 UPDATE，rule_versions 保持 pending 由管理员决定
        // 是否清理）。
        //
        // **F15-fix1**：finish lease guard 失败时不再 silent warn 然后返
        // `Ok(summary)` —— 那会让 CLI 误报成功而 rule_versions 实际仍 pending。
        // 改为返 `RuntimeError::LeaseConflict`，让 caller（CLI）以非零退出码
        // 终止并把责任明确移交给 admin。
        // F15-fix5/fix8：先把 finalize 与失败事件持久化跑完，再决定向上传播
        // 的错误。Err / finalize-lease-lost 两条失败路径都按
        // `docs-backup/design/error-and-observability.md` §4.3 line 317 的契约 emit
        // `run_failed`；只有真正成功（inner Ok + finalize 持有 lease）才发
        // `run_completed`。`mark_failed` 自身写入失败把 `persist_err` 折进
        // `run_failed.context_json` 而不是吞为日志（避免观测黑洞），但仍以
        // 原始 inner reindex error 作为返回值——控制面/因果链不变。
        let finished_at = OffsetDateTime::now_utc();
        let mut lease_lost_on_finalize = false;
        let mut mark_failed_persist_err: Option<String> = None;
        let mut mark_failed_no_update = false;
        match &inner_result {
            Ok(()) => {
                let outcome = self
                    .ctx
                    .reindex_job_repo
                    .finish_reindex_tx(job_id, &owner, rule_id, "reindex", finished_at)
                    .await?;
                if !outcome.job_completed {
                    tracing::warn!(
                        stage = "reindex",
                        target = %target_str,
                        job_id,
                        rule_version_id = rule_id,
                        owner = %owner,
                        "finish_reindex_tx lease guard 失败（可能已被 reclaim）；\
                         rule_versions 仍为 pending，需 admin 介入；CLI 将以 LeaseConflict 退出"
                    );
                    lease_lost_on_finalize = true;
                } else {
                    tracing::info!(
                        stage = "reindex",
                        target = %target_str,
                        job_id,
                        rule_version_id = rule_id,
                        demoted_rule_version_id = outcome.demoted_rule_version_id,
                        "reindex finalize：rule_versions pending → active{}",
                        if outcome.demoted_rule_version_id.is_some() {
                            " + 旧 active → superseded"
                        } else {
                            "（首版，无 demote）"
                        }
                    );
                }
            }
            Err(error) => {
                let error_repr = format!("{error}");
                match self
                    .ctx
                    .reindex_job_repo
                    .mark_failed(job_id, &owner, &error_repr, finished_at)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            stage = "reindex",
                            target = %target_str,
                            job_id,
                            owner = %owner,
                            "mark_failed 未更新行（lease 可能已过期或被 reclaim）"
                        );
                        mark_failed_no_update = true;
                    }
                    Err(persist_err) => {
                        tracing::error!(
                            stage = "reindex",
                            target = %target_str,
                            job_id,
                            owner = %owner,
                            ?persist_err,
                            "mark_failed 写入失败；折入 run_failed.context_json 后保留原始 reindex 错误向上抛"
                        );
                        mark_failed_persist_err = Some(format!("{persist_err:?}"));
                    }
                }
            }
        }

        if let Err(error) = &inner_result {
            emitter
                .emit(
                    "run_failed",
                    "error",
                    None,
                    None,
                    "reindex failed",
                    Some(json!({
                        "reindex_job_id": summary.reindex_job_id,
                        "rule_version_id": summary.new_rule_version_id,
                        "target": target_str,
                        "error": format!("{error}"),
                        "error_kind": rss_ai_news_domain::error::ClassifiedError::error_kind(error),
                        "mark_failed_no_update": mark_failed_no_update,
                        "mark_failed_persist_err": mark_failed_persist_err,
                    })),
                )
                .await;
        } else if lease_lost_on_finalize {
            emitter
                .emit(
                    "run_failed",
                    "error",
                    None,
                    None,
                    "reindex failed: finalize lease lost",
                    Some(json!({
                        "reindex_job_id": summary.reindex_job_id,
                        "rule_version_id": summary.new_rule_version_id,
                        "target": target_str,
                        "error": "finish_reindex_tx lease guard failed",
                        "error_kind": "lease_conflict",
                    })),
                )
                .await;
        }

        inner_result?;
        if lease_lost_on_finalize {
            return Err(RuntimeError::LeaseConflict {
                table: "reindex_jobs",
                id: job_id,
                expected_owner: owner.clone(),
            });
        }

        emitter
            .emit(
                "run_completed",
                "info",
                None,
                None,
                "reindex completed",
                Some(json!({
                    "reindex_job_id": summary.reindex_job_id,
                    "rule_version_id": summary.new_rule_version_id,
                    "scanned": summary.scanned,
                    "updated": summary.updated,
                    "unchanged": summary.unchanged,
                    "conflict_skipped": summary.conflict_skipped,
                    "archived": summary.archived,
                    "errors": summary.errors,
                })),
            )
            .await;
        Ok(summary)
    }

    /// 三类 target 的内部循环统一入口；外层 [`Self::run`] 负责 lease finalize。
    ///
    /// **F15-fix3**：原 `rule_id` 参数（reindex 自身的 kind='reindex' 行 id）
    /// 已从签名中移除——categories 现在内部查 `active kind='config'` 行，
    /// 不需要 reindex rule_id；link_hash / content_hash 本来就不依赖它，
    /// 仅靠 feed_entries / articles 自身的派生字段重算。
    async fn run_inner(
        &self,
        opts: &ReindexOptions,
        job_id: i64,
        owner: &str,
        after_id_start: i64,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        match opts.target {
            ReindexTarget::LinkHash => {
                self.reindex_link_hash(opts.batch_size, job_id, owner, after_id_start, summary)
                    .await
            }
            ReindexTarget::ContentHash => {
                self.reindex_content_hash(opts.batch_size, job_id, owner, after_id_start, summary)
                    .await
            }
            ReindexTarget::Categories => {
                // Categories 是一次性遍历配置（无 after_id 分页 / 无 checkpoint
                // 意义）；F15-fix7 起 lease 校验与 feed_sources 写在 storage
                // 层同 sqlx tx 里串联，关闭 fix2 残留的 guard→write TOCTOU 窗口。
                self.reindex_categories(opts.categories.clone(), job_id, owner, summary)
                    .await
            }
        }
    }

    /// F15-7 跨表 start TX + 按 id 领取自己刚建的 pending job（F15-8 W9-F3：
    /// 不走 `claim_pending` 的 `(created_at, id)` 扫描，避免拿错残留 job）。
    async fn start_new_job(
        &self,
        opts: &ReindexOptions,
        target_str: &str,
        owner: &str,
    ) -> Result<ClaimedReindexJob, RuntimeError> {
        let start = self
            .ctx
            .reindex_job_repo
            .start_reindex_tx(
                "reindex",
                &opts.new_rule_version_tag,
                &opts.new_rule_version_description,
                &opts.new_rule_version_sha256,
                target_str,
                OffsetDateTime::now_utc(),
            )
            .await?;
        let job_id = start.job_id;
        let claim_now = OffsetDateTime::now_utc();
        self.ctx
            .reindex_job_repo
            .claim_by_id(job_id, owner, claim_now, self.lease_deadline(claim_now))
            .await?
            .ok_or_else(|| {
                RuntimeError::Config(format!(
                    "reindex_job#{job_id} claim_by_id 未命中：start_reindex_tx 刚创建的 \
                     pending job 应当立即可 claim（owner={owner}）"
                ))
            })
    }

    fn lease_deadline(&self, now: OffsetDateTime) -> OffsetDateTime {
        lease_expires_at(
            now,
            Duration::seconds(self.ctx.lease.ai_duration_seconds as i64),
        )
    }

    /// 一批的 checkpoint 参数：`last_processed_id` 为本页最后扫描到的 id，lease
    /// 从现在起续租。批写入、owner 校验与 checkpoint 在 storage 同一事务内完成，
    /// 已失去 lease 的 worker 不会改动任何业务行（F15-fix2 的 guard 前移到写入前）。
    fn batch_checkpoint<'a>(
        &self,
        job_id: i64,
        owner: &'a str,
        last_processed_id: i64,
    ) -> ReindexBatchCheckpoint<'a> {
        let now = OffsetDateTime::now_utc();
        ReindexBatchCheckpoint {
            job_id,
            owner,
            last_processed_id,
            now,
            lease_expires_at: self.lease_deadline(now),
        }
    }

    /// F15-fix7：lease 校验失败统一映射为 `RuntimeError::LeaseConflict`，
    /// 与 reindex 路径其它 guard 失败的对外语义保持一致。
    fn lease_lost(job_id: i64, owner: &str) -> RuntimeError {
        tracing::warn!(
            stage = "reindex",
            job_id,
            owner = %owner,
            "lease-guarded write 拒绝：state≠'running' 或 lease_owner 不匹配；中止本 worker"
        );
        RuntimeError::LeaseConflict {
            table: "reindex_jobs",
            id: job_id,
            expected_owner: owner.to_string(),
        }
    }
}

/// An active job of the target blocks this run; nothing was changed.
fn active_job_conflict(target: &str, job_id: i64, detail: &str) -> RuntimeError {
    RuntimeError::Storage(StorageError::Conflict {
        table: "reindex_jobs".to_string(),
        key: format!(
            "target={target}: {detail}; resume it with the same rule, or run \
             `reindex --abort {job_id}` first"
        ),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}
