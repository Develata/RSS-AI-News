//! `reindex --dry-run`：仅扫描 + 内存等价计算，不调任何写 API。
//! 判别逻辑与 [`super::execute`] 的真实 run 完全一致，保证数字可信。

use std::collections::{BTreeSet, HashMap, HashSet};

use rss_ai_news_config::CategoryConfig;
use rss_ai_news_domain::link_normalizer::normalize_link;
use rss_ai_news_domain::state::FeedSourceStatus;
use rss_ai_news_storage::{ArticleRepository, FeedEntryRepository, LinkHashReindexCandidate};

use crate::error::RuntimeError;

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
        let target_str = opts.target.to_string();
        tracing::info!(
            run_id = %self.ctx.run.run_id,
            stage = "reindex",
            target = %target_str,
            dry_run = true,
            "reindex dry-run started",
        );

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

        tracing::info!(
            run_id = %self.ctx.run.run_id,
            stage = "reindex",
            target = %target_str,
            dry_run = true,
            scanned = summary.scanned,
            would_update = summary.updated,
            unchanged = summary.unchanged,
            conflict_skipped = summary.conflict_skipped,
            archived = summary.archived,
            errors = summary.errors,
            "reindex dry-run completed",
        );
        Ok(summary)
    }

    async fn dry_run_link_hash(
        &self,
        batch_size: u32,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let rows = load_link_hash_rows(
            self.ctx.feed_entry_repo.as_ref(),
            batch_size,
            MAX_HASH_DRY_RUN_ROWS,
        )
        .await?;

        // Real run 的每次 move 都在 storage transaction 内对 old/new 两组按
        // 最小 id 重选 canonical。Dry-run 先建立全表 group membership，再按
        // 相同 id 顺序模拟 move，故 collision chain 与真实 outcome 等价。
        let mut groups: HashMap<String, BTreeSet<i64>> = HashMap::new();
        for row in &rows {
            groups
                .entry(row.link_hash.clone())
                .or_default()
                .insert(row.id);
        }

        for row in rows {
            summary.scanned += 1;
            let normalized = match normalize_link(&row.normalized_link) {
                Ok(normalized) => normalized,
                Err(_) => {
                    summary.errors += 1;
                    continue;
                }
            };
            if normalized.link_hash == row.link_hash {
                summary.unchanged += 1;
                continue;
            }

            if let Some(old_group) = groups.get_mut(&row.link_hash) {
                old_group.remove(&row.id);
                if old_group.is_empty() {
                    groups.remove(&row.link_hash);
                }
            }
            let new_group = groups.entry(normalized.link_hash).or_default();
            new_group.insert(row.id);
            if new_group.first().copied() == Some(row.id) {
                summary.updated += 1;
            } else {
                summary.conflict_skipped += 1;
            }
        }
        Ok(())
    }

    async fn dry_run_content_hash(
        &self,
        batch_size: u32,
        summary: &mut ReindexSummary,
    ) -> Result<(), RuntimeError> {
        let mut occupied = load_content_hashes(
            self.ctx.article_repo.as_ref(),
            batch_size,
            MAX_HASH_DRY_RUN_ROWS,
        )
        .await?;
        let mut after_id = 0i64;
        loop {
            let rows = self
                .ctx
                .article_repo
                .list_for_content_hash_reindex(
                    after_id,
                    batch_size.clamp(1, MAX_DRY_RUN_BATCH_ROWS),
                )
                .await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                if summary.scanned as usize >= MAX_HASH_DRY_RUN_ROWS {
                    return Err(snapshot_limit_error("content_hash", MAX_HASH_DRY_RUN_ROWS));
                }
                after_id = row.id;
                summary.scanned += 1;
                let new_hash = sha256_hex(row.body_text.as_bytes());
                if new_hash == row.content_hash {
                    summary.unchanged += 1;
                    continue;
                }
                if occupied.contains(&new_hash) {
                    summary.conflict_skipped += 1;
                } else {
                    occupied.remove(&row.content_hash);
                    occupied.insert(new_hash);
                    summary.updated += 1;
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
                configured.insert((category.category.key.as_str(), source.key.as_str()));
                summary.scanned += 1;
                summary.updated += 1;
            }
        }
        for source in existing {
            if !configured.contains(&(source.category_key.as_str(), source.source_key.as_str()))
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

// Collision-chain simulation needs every existing hash group. Refuse oversized
// snapshots explicitly instead of silently producing incomplete counts.
const MAX_HASH_DRY_RUN_ROWS: usize = 100_000;
const MAX_DRY_RUN_BATCH_ROWS: u32 = 1024;

async fn load_link_hash_rows(
    repo: &dyn FeedEntryRepository,
    batch_size: u32,
    row_limit: usize,
) -> Result<Vec<LinkHashReindexCandidate>, RuntimeError> {
    let mut after_id = 0;
    let mut rows = Vec::new();
    loop {
        let batch = repo
            .list_for_link_hash_reindex(after_id, batch_size.clamp(1, MAX_DRY_RUN_BATCH_ROWS))
            .await?;
        if batch.is_empty() {
            return Ok(rows);
        }
        if rows.len().saturating_add(batch.len()) > row_limit {
            return Err(snapshot_limit_error("link_hash", row_limit));
        }
        after_id = batch.last().expect("non-empty batch").id;
        rows.extend(batch);
    }
}

async fn load_content_hashes(
    repo: &dyn ArticleRepository,
    batch_size: u32,
    row_limit: usize,
) -> Result<HashSet<String>, RuntimeError> {
    let mut after_id = 0;
    let mut row_count = 0_usize;
    let mut hashes = HashSet::new();
    loop {
        let batch = repo
            .list_content_hashes(after_id, batch_size.clamp(1, MAX_DRY_RUN_BATCH_ROWS))
            .await?;
        if batch.is_empty() {
            return Ok(hashes);
        }
        row_count = row_count.saturating_add(batch.len());
        if row_count > row_limit {
            return Err(snapshot_limit_error("content_hash", row_limit));
        }
        after_id = batch.last().expect("non-empty batch").id;
        hashes.extend(batch.into_iter().map(|row| row.content_hash));
    }
}

fn snapshot_limit_error(target: &str, row_limit: usize) -> RuntimeError {
    RuntimeError::Config(format!(
        "{target} dry-run exceeds the safety limit of {row_limit} rows; \
         no partial report was produced; use a normal reindex to process larger databases in batches"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rss_ai_news_storage::{ArticleRepo, FeedEntryRepo};
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn link_hash_snapshot_limit_accepts_exact_boundary_and_rejects_overflow() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE feed_entries (id INTEGER PRIMARY KEY, normalized_link TEXT NOT NULL, link_hash TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO feed_entries VALUES \
             (1, 'https://example.test/1', 'one'), \
             (2, 'https://example.test/2', 'two'), \
             (3, 'https://example.test/3', 'three')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let repo = FeedEntryRepo::new(pool.clone());
        for batch_size in [1, u32::MAX] {
            let rows = load_link_hash_rows(&repo, batch_size, 3).await.unwrap();
            assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), [1, 2, 3]);
            let error = load_link_hash_rows(&repo, batch_size, 2).await.unwrap_err();
            assert!(error.to_string().contains("safety limit of 2 rows"));
            assert!(error.to_string().contains("no partial report"));
        }
        pool.close().await;
    }
    #[tokio::test]
    async fn content_hash_snapshot_is_narrow_and_bounded() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // No body column: collecting hash ownership must not load article content.
        sqlx::query("CREATE TABLE articles (id INTEGER PRIMARY KEY, content_hash TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO articles VALUES (1, 'one'), (2, 'two'), (3, 'three')")
            .execute(&pool)
            .await
            .unwrap();
        let repo = ArticleRepo::new(pool.clone());
        for batch_size in [1, u32::MAX] {
            let hashes = load_content_hashes(&repo, batch_size, 3).await.unwrap();
            assert_eq!(hashes.len(), 3);
            assert!(hashes.contains("one") && hashes.contains("two") && hashes.contains("three"));
            let error = load_content_hashes(&repo, batch_size, 2).await.unwrap_err();
            assert!(error.to_string().contains("safety limit of 2 rows"));
            assert!(error.to_string().contains("no partial report"));
        }
        pool.close().await;
    }
}
