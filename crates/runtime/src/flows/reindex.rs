use std::{collections::HashSet, sync::Arc};

use rss_ai_news_config::CategoryConfig;
pub use rss_ai_news_domain::state::ReindexTarget;
use rss_ai_news_domain::{
    link_normalizer::normalize_link, model::FeedSource, state::FeedSourceStatus,
};
use rss_ai_news_storage::UpdateContentHashOutcome;
use serde_json::json;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

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
        // 推进到 'active'（旧 active → 'superseded'）；reindex_jobs 的
        // 'pending' → 'completed' 在本提交里临时由 `complete_without_claim`
        // 闭环，F15-8 引入 claim+checkpoint、F15-9 引入跨表 finish TX
        // 后该路径会被替换。
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

        let mut summary = ReindexSummary {
            new_rule_version_id: rule_id,
            reindex_job_id: job_id,
            ..ReindexSummary::default()
        };
        match opts.target {
            ReindexTarget::LinkHash => {
                self.reindex_link_hash(opts.batch_size, &mut summary)
                    .await?
            }
            ReindexTarget::ContentHash => {
                self.reindex_content_hash(opts.batch_size, &mut summary)
                    .await?;
            }
            ReindexTarget::Categories => {
                self.reindex_categories(opts.categories, rule_id, &mut summary)
                    .await?;
            }
        }

        // F15-7 过渡：成功路径把 reindex_jobs 行直接推到 completed，避免
        // 下一次同 target reindex 被 partial unique `uq_reindex_jobs_target_active`
        // 拒绝。失败路径（提前 `?` 返）下 job 留在 pending，需用户
        // `reindex --abort` 清理（F15-10）。F15-9 finish TX 接入后这段
        // 会被 claim_pending + advance_to_completed 的跨表事务替换。
        self.ctx
            .reindex_job_repo
            .complete_without_claim(job_id, OffsetDateTime::now_utc())
            .await?;

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

    async fn reindex_link_hash(
        &self,
        batch_size: u32,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let mut after_id = 0;
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
        }
        Ok(())
    }

    async fn reindex_content_hash(
        &self,
        batch_size: u32,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let mut after_id = 0;
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
