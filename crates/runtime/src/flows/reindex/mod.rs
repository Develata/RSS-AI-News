//! reindex flow：rebuild link_hash / content_hash / feed_sources（categories）。
//!
//! 本文件持 lease 生命周期编排（start/claim/run_inner/finalize）+ checkpoint +
//! sha256 工具；abort 见 [`abort`]，dry-run 见 [`dry_run`]，真实写入循环见
//! [`execute`]，DTO 见 [`dto`]。

use std::sync::Arc;

use rss_ai_news_storage::{build_owner_id, lease_expires_at};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::context::RunContext;
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
    ctx: Arc<RunContext>,
}

impl ReindexFlow {
    pub fn new(ctx: Arc<RunContext>) -> Self {
        Self { ctx }
    }

    pub async fn run(&self, opts: ReindexOptions) -> Result<ReindexSummary, RuntimeError> {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
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

        // F15-7 W9-F4: 跨表 start TX —— rule_versions(status='pending') +
        // reindex_jobs(state='pending') 同事务写入；任一冲突整段回滚，
        // 避免 rule_versions 留"孤儿 pending"行或同 target 启动两条活动 job。
        // rule_versions 的 status='pending' 在 F15-9 finish TX 接入后会被
        // 推进到 'active'（旧 active → 'superseded'）。
        let started_at = OffsetDateTime::now_utc();
        let target_str = opts.target.to_string();
        let start = self
            .ctx
            .reindex_job_repo
            .start_reindex_tx(
                "reindex",
                &opts.new_rule_version_tag,
                &opts.new_rule_version_description,
                &opts.new_rule_version_sha256,
                &target_str,
                started_at,
            )
            .await?;
        let rule_id = start.rule_version_id;
        let job_id = start.job_id;

        // F15-8 W9-F3: 按 id 寻址 claim 自己刚 INSERT 的 pending job——
        // 不能走 claim_pending 的 `(created_at ASC, id ASC)` 扫描，否则在
        // 库里残留旧 pending 时会拿错 job。claim_by_id 把 pending → running，
        // 写 lease（lease_owner / lease_expires_at）+ started_at(COALESCE) +
        // attempt_count += 1。lease 时长复用 `lease.ai_duration_seconds`
        // （reindex 工作负载与 AI 接近：长批处理 + 大量 I/O；独立的
        // `reindex_duration_seconds` 字段留给后续 config schema 演进）。
        let owner = build_owner_id();
        let claim_now = OffsetDateTime::now_utc();
        let claim_lease = lease_expires_at(
            claim_now,
            Duration::seconds(self.ctx.app.lease.ai_duration_seconds as i64),
        );
        let claimed = self
            .ctx
            .reindex_job_repo
            .claim_by_id(job_id, &owner, claim_now, claim_lease)
            .await?
            .ok_or_else(|| {
                RuntimeError::Config(format!(
                    "reindex_job#{job_id} claim_by_id 未命中：start_reindex_tx 刚创建的 \
                     pending job 应当立即可 claim（owner={owner}）"
                ))
            })?;

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
        // `docs/design/error-and-observability.md` §4.3 line 317 的契约 emit
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

    /// 把 `last_processed_id` 推进到 SQLite（reindex_jobs.advance_checkpoint）。
    /// guard 失败（`lease_owner != owner` 或 `state != 'running'`）说明
    /// abort / reclaim 已经发生——本 worker 没资格继续推进。
    ///
    /// **F15-fix2**：以前在 guard 失败时只 warn 然后 `Ok(())` 继续下一批；
    /// 那会让旧 worker 在 lease 已易主后继续写数据、与新 worker 的写入
    /// 竞争（abort 会立刻释放 `uq_reindex_jobs_target_active`，新 job
    /// 可同 target 重启）。改为直接返 `RuntimeError::LeaseConflict`，让
    /// 外层 `run()` 走 Err 分支 → mark_failed（lease 已失会 no-op，符合
    /// 预期）→ 原始 LeaseConflict 向上抛 → CLI 非零退出。
    async fn checkpoint(
        &self,
        job_id: i64,
        owner: &str,
        last_processed_id: i64,
    ) -> Result<(), RuntimeError> {
        let updated = self
            .ctx
            .reindex_job_repo
            .advance_checkpoint(job_id, owner, last_processed_id, OffsetDateTime::now_utc())
            .await?;
        if !updated {
            tracing::warn!(
                stage = "reindex",
                job_id,
                owner = %owner,
                last_processed_id,
                "advance_checkpoint guard 失败：state≠'running' 或 lease_owner 不匹配；中止本 worker 后续批次写入"
            );
            return Err(RuntimeError::LeaseConflict {
                table: "reindex_jobs",
                id: job_id,
                expected_owner: owner.to_string(),
            });
        }
        Ok(())
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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}
