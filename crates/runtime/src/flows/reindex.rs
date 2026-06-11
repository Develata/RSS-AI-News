use std::{collections::HashSet, sync::Arc};

use rss_ai_news_config::CategoryConfig;
pub use rss_ai_news_domain::state::ReindexTarget;
use rss_ai_news_domain::{
    link_normalizer::normalize_link, model::FeedSource, state::FeedSourceStatus,
};
use rss_ai_news_storage::{
    LeaseGuardedWriteOutcome, UpdateContentHashOutcome, build_owner_id, lease_expires_at,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::context::RunContext;
use crate::error::RuntimeError;
use crate::events::RunEventEmitter;
use crate::flows::maintenance::emit_maintenance_outcome;

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

/// [`ReindexFlow::abort`] 返回值。`aborted=true` 表示 storage 真把状态从
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

pub struct ReindexFlow {
    ctx: Arc<RunContext>,
}

impl ReindexFlow {
    pub fn new(ctx: Arc<RunContext>) -> Self {
        Self { ctx }
    }

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

    /// `reindex --dry-run` 真路径（cli-semantics §4.8 line 289 / 325）：
    /// 不调任何写 API——
    ///   - **不**调 `start_reindex_tx`（rule_versions / reindex_jobs 都不写）
    ///   - **不**调 `claim_by_id` / `advance_checkpoint` / `finish_reindex_tx`
    ///   - **不**调 `update_link_hash` / `update_content_hash` / `upsert` /
    ///     `mark_archived`
    ///
    /// 仅扫描候选行 + 内存等价计算，复用与 [`Self::run`] 完全一致的判别
    /// 逻辑（normalize_link / sha256 / configured 集合差集），所以 dry-run
    /// 与真实 run 的 scanned/updated/unchanged/conflict_skipped/archived/
    /// errors 数字应当一致——这是 doc §4.8 line 325 "Would update N rows"
    /// 可信度的基础。
    ///
    /// 返回值中 `new_rule_version_id = 0` 且 `reindex_job_id = 0` 标记
    /// dry-run；CLI 借此识别并跳过 job_id 行的 pretty 输出。
    pub async fn dry_run(&self, opts: ReindexOptions) -> Result<ReindexSummary, RuntimeError> {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "reindex",
            repo: self.ctx.event_repo.as_ref(),
        };
        let target_str = opts.target.to_string();
        emitter
            .emit(
                "run_started",
                "info",
                None,
                None,
                "reindex dry-run started",
                Some(json!({
                    "target": target_str,
                    "dry_run": true,
                    "rule_version_tag": opts.new_rule_version_tag,
                })),
            )
            .await;

        let mut summary = ReindexSummary::default();
        match opts.target {
            ReindexTarget::LinkHash => {
                self.dry_run_link_hash(opts.batch_size, &mut summary)
                    .await?
            }
            ReindexTarget::ContentHash => {
                self.dry_run_content_hash(opts.batch_size, &mut summary)
                    .await?
            }
            ReindexTarget::Categories => {
                self.dry_run_categories(&opts.categories, &mut summary)
                    .await?
            }
        }

        emitter
            .emit(
                "run_completed",
                "info",
                None,
                None,
                "reindex dry-run completed",
                Some(json!({
                    "target": target_str,
                    "dry_run": true,
                    "scanned": summary.scanned,
                    "would_update": summary.updated,
                    "unchanged": summary.unchanged,
                    "conflict_skipped": summary.conflict_skipped,
                    "archived": summary.archived,
                    "errors": summary.errors,
                })),
            )
            .await;
        Ok(summary)
    }

    async fn dry_run_link_hash(
        &self,
        batch_size: u32,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let mut after_id = 0i64;
        loop {
            let rows = self
                .ctx
                .feed_entry_repo
                .list_for_link_hash_reindex(after_id, batch_size.max(1))
                .await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                after_id = row.id;
                summary.scanned += 1;
                match normalize_link(&row.normalized_link) {
                    Ok(normalized) => {
                        if normalized.link_hash == row.link_hash {
                            summary.unchanged += 1;
                        } else {
                            summary.updated += 1;
                        }
                    }
                    Err(_) => summary.errors += 1,
                }
            }
        }
        Ok(())
    }

    async fn dry_run_content_hash(
        &self,
        batch_size: u32,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let mut after_id = 0i64;
        loop {
            let rows = self
                .ctx
                .article_repo
                .list_for_content_hash_reindex(after_id, batch_size.max(1))
                .await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                after_id = row.id;
                summary.scanned += 1;
                let new_hash = sha256_hex(row.body_text.as_bytes());
                if new_hash == row.content_hash {
                    summary.unchanged += 1;
                    continue;
                }
                match self
                    .ctx
                    .article_repo
                    .peek_content_hash_outcome(row.id, &new_hash)
                    .await?
                {
                    UpdateContentHashOutcome::Updated => summary.updated += 1,
                    UpdateContentHashOutcome::Conflict => summary.conflict_skipped += 1,
                    UpdateContentHashOutcome::Unchanged => summary.unchanged += 1,
                }
            }
        }
        Ok(())
    }

    async fn dry_run_categories(
        &self,
        categories: &[CategoryConfig],
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        // Categories dry-run：与 reindex_categories 共享 configured 集合差
        // 集逻辑，但 scanned 仍按 reindex_categories 实际行为递增——
        // configured 中每个 source 计 scanned+updated 一次（真实 run 中
        // upsert 一律视为 updated，即便底层无变化）。
        let existing = self.ctx.feed_source_repo.list_all().await?;
        let mut configured = HashSet::new();
        for category in categories {
            for source in &category.sources {
                configured.insert((category.category.key.clone(), source.key.clone()));
                summary.scanned += 1;
                summary.updated += 1;
            }
        }
        for source in existing {
            if !configured.contains(&(source.category_key.clone(), source.source_key.clone()))
                && matches!(
                    source.status,
                    FeedSourceStatus::Active | FeedSourceStatus::Paused
                )
            {
                summary.archived += 1;
            }
        }
        Ok(())
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

    async fn reindex_link_hash(
        &self,
        batch_size: u32,
        job_id: i64,
        owner: &str,
        after_id_start: i64,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let mut after_id = after_id_start;
        loop {
            let rows = self
                .ctx
                .feed_entry_repo
                .list_for_link_hash_reindex(after_id, batch_size.max(1))
                .await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                after_id = row.id;
                summary.scanned += 1;
                let normalized = match normalize_link(&row.normalized_link) {
                    Ok(value) => value,
                    Err(_) => {
                        summary.errors += 1;
                        continue;
                    }
                };
                if normalized.link_hash == row.link_hash {
                    summary.unchanged += 1;
                } else if self
                    .ctx
                    .feed_entry_repo
                    .update_link_hash(row.id, &normalized.link_hash)
                    .await?
                {
                    summary.updated += 1;
                } else {
                    summary.errors += 1;
                }
            }
            self.checkpoint(job_id, owner, after_id).await?;
        }
        Ok(())
    }

    async fn reindex_content_hash(
        &self,
        batch_size: u32,
        job_id: i64,
        owner: &str,
        after_id_start: i64,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let mut after_id = after_id_start;
        loop {
            let rows = self
                .ctx
                .article_repo
                .list_for_content_hash_reindex(after_id, batch_size.max(1))
                .await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                after_id = row.id;
                summary.scanned += 1;
                let new_hash = sha256_hex(row.body_text.as_bytes());
                if new_hash == row.content_hash {
                    summary.unchanged += 1;
                    continue;
                }
                match self
                    .ctx
                    .article_repo
                    .update_content_hash(row.id, &new_hash)
                    .await?
                {
                    UpdateContentHashOutcome::Updated => summary.updated += 1,
                    UpdateContentHashOutcome::Conflict => summary.conflict_skipped += 1,
                    UpdateContentHashOutcome::Unchanged => summary.unchanged += 1,
                }
            }
            self.checkpoint(job_id, owner, after_id).await?;
        }
        Ok(())
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

    async fn reindex_categories(
        &self,
        categories: Vec<CategoryConfig>,
        job_id: i64,
        owner: &str,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        // F15-fix3：feed_sources.config_version 必须指向 `kind='config'` 的
        // rule_versions 行——FK 只声明在 rule_versions(id) 上，但语义上该列
        // 与 config TOML 快照对齐。reindex flow 自己创建的是 `kind='reindex'`
        // 行（作为 reindex 操作的审计 manifest），把它写入 config_version
        // 会让下游 active_rule_or_register('config') 反查不到对应 row 的
        // payload_sha256，破坏 ingest/extract/ai/publish 读路径的版本一致性。
        //
        // 用 `active_rule_or_register("config", ...)` 拿当前 active config 行；
        // 生产环境下 CLI 启动期 `ensure_active_config_version`（W16，
        // docs/plan/16-config-versioning.md §5）已把 active 行轮换到当前真实
        // config_sha256，本调用走"查现有 active"分支；测试环境下若无 active
        // config 行则 seed placeholder（version_tag 显式标为
        // `reindex-categories-bootstrap` 让 admin 一眼看出是回退路径，下次
        // CLI 启动被 rotate 收编）。
        let config_version_id = self
            .ctx
            .rule_version_repo
            .active_rule_or_register(
                "config",
                "reindex-categories-bootstrap",
                "auto-registered by reindex categories when no active config rule existed",
                "reindex-categories-bootstrap",
            )
            .await?;

        let existing = self.ctx.feed_source_repo.list_all().await?;
        let mut configured = HashSet::new();

        // F15-fix9：循环外不再 cache 一个 `now`。每次写操作都重新取
        // `OffsetDateTime::now_utc()`，让传入 storage 的 lease guard UPDATE
        // 写到 `reindex_jobs.updated_at` 的时间戳跟随实际写入瞬时——这条
        // 列是 reclaim 巡检判断"worker 是否仍活跃"的 heartbeat。
        for category in categories {
            for source in category.sources {
                configured.insert((category.category.key.clone(), source.key.clone()));
                let now = OffsetDateTime::now_utc();
                let feed_source = FeedSource {
                    id: 0,
                    category_key: category.category.key.clone(),
                    source_key: source.key,
                    display_name: source.display_name,
                    feed_url: source.feed_url,
                    feed_kind: source.feed_kind,
                    status: if source.enabled {
                        FeedSourceStatus::Active
                    } else {
                        FeedSourceStatus::Paused
                    },
                    priority: i64::from(source.priority),
                    etag: None,
                    last_modified: None,
                    last_fetched_at: None,
                    last_success_at: None,
                    consecutive_failures: 0,
                    last_error: None,
                    last_error_kind: None,
                    config_version: config_version_id,
                    created_at: now,
                    updated_at: now,
                };
                // F15-fix7：lease 校验 + upsert 在 storage 层同事务执行，
                // 关闭 fix2 残留的 guard→write TOCTOU 窗口。lease 已失则
                // 整段事务 rollback，feed_sources 行不被覆盖，直接 LeaseConflict
                // 退出走 outer run() 的 mark_failed 路径。
                match self
                    .ctx
                    .feed_source_repo
                    .upsert_with_lease_guard(&feed_source, job_id, owner, now)
                    .await?
                {
                    LeaseGuardedWriteOutcome::Applied => {
                        summary.scanned += 1;
                        summary.updated += 1;
                    }
                    LeaseGuardedWriteOutcome::NoOp => {
                        // upsert 路径在 lease 在手时永远会改一行（INSERT 或
                        // UPDATE），NoOp 状态不可达；进入此分支说明 storage
                        // 实现破坏了语义契约，留 unreachable! 兜底。
                        unreachable!(
                            "upsert_with_lease_guard 返回 NoOp 违反契约：feed_sources \
                             upsert 在 lease 在手时必然写一行"
                        );
                    }
                    LeaseGuardedWriteOutcome::LeaseLost => {
                        return Err(Self::lease_lost(job_id, owner));
                    }
                }
            }
        }

        for source in existing {
            if !configured.contains(&(source.category_key, source.source_key)) {
                let now = OffsetDateTime::now_utc();
                match self
                    .ctx
                    .feed_source_repo
                    .mark_archived_with_lease_guard(source.id, job_id, owner, now)
                    .await?
                {
                    LeaseGuardedWriteOutcome::Applied => summary.archived += 1,
                    // NoOp：行已是 archived。沿用 fix2 之前 `mark_archived`
                    // 返 `false` 的语义，不递增 summary.archived。
                    LeaseGuardedWriteOutcome::NoOp => {}
                    LeaseGuardedWriteOutcome::LeaseLost => {
                        return Err(Self::lease_lost(job_id, owner));
                    }
                }
            }
        }
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}
