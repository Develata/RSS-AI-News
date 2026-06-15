//! `reindex --dry-run`：仅扫描 + 内存等价计算，不调任何写 API。
//! 判别逻辑与 [`super::execute`] 的真实 run 完全一致，保证数字可信。

use std::collections::HashSet;

use rss_ai_news_config::CategoryConfig;
use rss_ai_news_domain::link_normalizer::normalize_link;
use rss_ai_news_domain::state::FeedSourceStatus;
use rss_ai_news_storage::UpdateContentHashOutcome;
use serde_json::json;

use crate::error::RuntimeError;
use crate::events::RunEventEmitter;

use super::sha256_hex;
use super::{ReindexFlow, ReindexOptions, ReindexSummary, ReindexTarget};

impl ReindexFlow {
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
}
