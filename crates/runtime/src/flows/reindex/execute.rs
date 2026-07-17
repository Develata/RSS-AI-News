//! 三类 target 的真实重算 / 写入循环：link_hash / content_hash / categories。
//! 由 [`super::ReindexFlow::run`] 经 `run_inner` 分派；checkpoint / lease_lost /
//! lease finalize 由父模块负责。

use std::collections::HashSet;

use rss_ai_news_config::CategoryConfig;
use rss_ai_news_domain::link_normalizer::normalize_link;
use rss_ai_news_domain::model::FeedSource;
use rss_ai_news_domain::state::FeedSourceStatus;
use rss_ai_news_storage::{
    LeaseGuardedWriteOutcome, UpdateContentHashOutcome, UpdateLinkHashOutcome,
};
use time::OffsetDateTime;

use crate::error::RuntimeError;

use super::sha256_hex;
use super::{ReindexFlow, ReindexSummary};

impl ReindexFlow {
    pub(super) async fn reindex_link_hash(
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
                } else {
                    match self
                        .ctx
                        .feed_entry_repo
                        .update_link_hash(row.id, &normalized.link_hash)
                        .await?
                    {
                        UpdateLinkHashOutcome::Updated => summary.updated += 1,
                        UpdateLinkHashOutcome::ConflictShadowed => summary.conflict_skipped += 1,
                        UpdateLinkHashOutcome::Missing => summary.errors += 1,
                    }
                }
            }
            self.checkpoint(job_id, owner, after_id).await?;
        }
        Ok(())
    }

    pub(super) async fn reindex_content_hash(
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

    pub(super) async fn reindex_categories(
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
