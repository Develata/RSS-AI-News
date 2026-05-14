use std::{collections::HashSet, sync::Arc};

use rss_ai_news_config::CategoryConfig;
pub use rss_ai_news_domain::state::ReindexTarget;
use rss_ai_news_domain::{
    link_normalizer::normalize_link, model::FeedSource, state::FeedSourceStatus,
};
use rss_ai_news_storage::{UpdateContentHashOutcome, build_owner_id, lease_expires_at};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::context::RunContext;
use crate::error::RuntimeError;
use crate::events::RunEventEmitter;

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
    /// 寻址用）；F15-9 finish TX 也会用此 id 推进跨表激活。
    pub reindex_job_id: i64,
    pub scanned: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub conflict_skipped: u32,
    pub archived: u32,
    pub errors: u32,
}

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
            .run_inner(&opts, job_id, &owner, rule_id, after_id_start, &mut summary)
            .await;

        // F15-9 W9-F4 finalize：成功 → finish_reindex_tx（跨表事务，单事务内
        // 推进 reindex_jobs running → completed + 旧 active rule_versions →
        // superseded + pending rule_versions → active）；失败 → mark_failed
        // （reindex_jobs 单表 UPDATE，rule_versions 保持 pending 由管理员决定
        // 是否清理）。两条路径都带 lease guard，guard 失败 → warn 不 Err。
        let finished_at = OffsetDateTime::now_utc();
        match &inner_result {
            Ok(()) => {
                let outcome = self
                    .ctx
                    .reindex_job_repo
                    .finish_reindex_tx(job_id, &owner, rule_id, "reindex", finished_at)
                    .await?;
                if !outcome.job_completed {
                    // lease guard 失败：整段事务回滚，rule_versions 保持 pending；
                    // 数据写已发生且幂等，正确性不依赖本次 finalize。
                    tracing::warn!(
                        stage = "reindex",
                        target = %target_str,
                        job_id,
                        rule_version_id = rule_id,
                        owner = %owner,
                        "finish_reindex_tx lease guard 失败（可能已被 reclaim）；\
                         rule_versions 仍为 pending，需由管理员介入"
                    );
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
                    Ok(false) => tracing::warn!(
                        stage = "reindex",
                        target = %target_str,
                        job_id,
                        owner = %owner,
                        "mark_failed 未更新行（lease 可能已过期或被 reclaim）"
                    ),
                    Err(persist_err) => tracing::error!(
                        stage = "reindex",
                        target = %target_str,
                        job_id,
                        owner = %owner,
                        ?persist_err,
                        "mark_failed 写入失败；保留原始 reindex 错误向上抛"
                    ),
                }
            }
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
        inner_result?;
        Ok(summary)
    }

    /// 三类 target 的内部循环统一入口；外层 [`Self::run`] 负责 lease finalize。
    async fn run_inner(
        &self,
        opts: &ReindexOptions,
        job_id: i64,
        owner: &str,
        rule_id: i64,
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
                // 意义）；lease 由外层 run() finalize。
                self.reindex_categories(opts.categories.clone(), rule_id, summary)
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
    /// guard 失败（`lease_owner != owner` 或 `state != 'running'`）只发 warn
    /// 不返 Err——若 lease 真的被 reclaim，reindex flow 的写已经发生且幂等，
    /// 数据正确性不依赖本次 checkpoint；F15-11 reclaim 后台 + F15-12 crash
    /// recovery 测试会进一步覆盖这条路径。
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
                "advance_checkpoint guard 失败：state≠'running' 或 lease_owner 不匹配（可能被 reclaim）"
            );
        }
        Ok(())
    }

    async fn reindex_categories(
        &self,
        categories: Vec<CategoryConfig>,
        rule_id: i64,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let existing = self.ctx.feed_source_repo.list_all().await?;
        let mut configured = HashSet::new();
        let now = OffsetDateTime::now_utc();

        for category in categories {
            for source in category.sources {
                configured.insert((category.category.key.clone(), source.key.clone()));
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
                    config_version: rule_id,
                    created_at: now,
                    updated_at: now,
                };
                self.ctx.feed_source_repo.upsert(&feed_source).await?;
                summary.scanned += 1;
                summary.updated += 1;
            }
        }

        for source in existing {
            if !configured.contains(&(source.category_key, source.source_key))
                && self.ctx.feed_source_repo.mark_archived(source.id).await?
            {
                summary.archived += 1;
            }
        }
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}
