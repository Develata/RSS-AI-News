use std::sync::Arc;
use std::time::Duration as StdDuration;

use rss_ai_news_config::{CategoryConfig, SourceConfig};
use rss_ai_news_domain::dto::feed::{FeedEntryMeta, FeedFetchRequest};
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_domain::link_normalizer::normalize_link;
use rss_ai_news_domain::model::FeedSource;
use rss_ai_news_domain::state::{FeedKind, FeedSourceStatus};
use rss_ai_news_feed::parse_feed;
use rss_ai_news_storage::NewFeedEntry;
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::artifact::ArtifactWriter;
use crate::context::RunContext;
use crate::events::RunEventEmitter;

#[derive(Debug, Clone, Default)]
pub struct IngestOptions {
    pub category_keys: Vec<String>,
    pub max_sources: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct IngestSummary {
    pub sources_attempted: u32,
    pub sources_succeeded: u32,
    pub sources_not_modified: u32,
    pub sources_failed: u32,
    pub entries_discovered: u32,
    pub entries_inserted: u32,
    pub entries_uid_dup: u32,
    pub entries_link_dup: u32,
    pub per_source: Vec<IngestSourceOutcome>,
}

#[derive(Debug, Clone)]
pub struct IngestSourceOutcome {
    pub source_id: i64,
    pub category_key: String,
    pub source_key: String,
    pub status: IngestSourceStatus,
    pub entries_discovered: u32,
    pub entries_inserted: u32,
    pub entries_uid_dup: u32,
    pub entries_link_dup: u32,
    pub error_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestSourceStatus {
    Succeeded,
    NotModified,
    Failed,
}

#[derive(Debug, Clone)]
struct SourceTask {
    category_key: String,
    source_key: String,
    source_id: i64,
    existing_etag: Option<String>,
    existing_last_modified: Option<String>,
    feed_url: String,
    feed_kind: FeedKind,
}

pub struct IngestFlow {
    ctx: Arc<RunContext>,
    categories: Vec<CategoryConfig>,
}

impl IngestFlow {
    pub fn new(ctx: Arc<RunContext>, categories: Vec<CategoryConfig>) -> Self {
        Self { ctx, categories }
    }

    pub async fn run(&self, opts: IngestOptions) -> IngestSummary {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: &self.ctx.stage,
            repo: self.ctx.event_repo.as_ref(),
        };
        emitter
            .emit(
                "run_started",
                "info",
                None,
                None,
                "ingest run started",
                None,
            )
            .await;

        let mut summary = IngestSummary::default();
        let source_configs = self.collect_enabled_sources(&opts);
        let mut tasks = Vec::with_capacity(source_configs.len());

        for (category, source) in source_configs {
            match self.resolve_source(&category, &source).await {
                Ok(task) => tasks.push(task),
                Err(error) => {
                    tracing::warn!(
                        category_key = %category.category.key,
                        source_key = %source.key,
                        "failed to resolve feed source: {error}"
                    );
                    summary.per_source.push(IngestSourceOutcome {
                        source_id: 0,
                        category_key: category.category.key.clone(),
                        source_key: source.key.clone(),
                        status: IngestSourceStatus::Failed,
                        entries_discovered: 0,
                        entries_inserted: 0,
                        entries_uid_dup: 0,
                        entries_link_dup: 0,
                        error_kind: Some(error.error_kind().to_string()),
                    });
                }
            }
        }

        summary.sources_attempted = (tasks.len() + summary.per_source.len()) as u32;
        let concurrent_feeds = self.ctx.app.http.concurrent_feeds.max(1) as usize;
        let semaphore = Arc::new(Semaphore::new(concurrent_feeds));
        let mut join_set = JoinSet::new();

        for task in tasks {
            let ctx = Arc::clone(&self.ctx);
            let semaphore = Arc::clone(&semaphore);
            join_set.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("semaphore should not be closed");
                Self::process_source(ctx, task).await
            });
        }

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(outcome) => summary.per_source.push(outcome),
                Err(error) => {
                    tracing::error!("ingest source task panicked or was cancelled: {error}");
                    summary.sources_failed += 1;
                }
            }
        }

        recalculate_summary(&mut summary);
        emitter
            .emit(
                "run_completed",
                "info",
                None,
                None,
                "ingest run completed",
                Some(json!({
                    "sources_attempted": summary.sources_attempted,
                    "sources_succeeded": summary.sources_succeeded,
                    "sources_not_modified": summary.sources_not_modified,
                    "sources_failed": summary.sources_failed,
                    "entries_discovered": summary.entries_discovered,
                    "entries_inserted": summary.entries_inserted,
                    "entries_uid_dup": summary.entries_uid_dup,
                    "entries_link_dup": summary.entries_link_dup,
                })),
            )
            .await;

        summary
    }

    fn collect_enabled_sources(&self, opts: &IngestOptions) -> Vec<(CategoryConfig, SourceConfig)> {
        let mut sources = Vec::new();
        for category in &self.categories {
            if !opts.category_keys.is_empty()
                && !opts
                    .category_keys
                    .iter()
                    .any(|key| key == &category.category.key)
            {
                continue;
            }
            for source in &category.sources {
                if source.enabled {
                    sources.push((category.clone(), source.clone()));
                }
            }
        }
        if let Some(limit) = opts.max_sources {
            sources.truncate(limit);
        }
        sources
    }

    async fn resolve_source(
        &self,
        category: &CategoryConfig,
        source: &SourceConfig,
    ) -> Result<SourceTask, rss_ai_news_storage::StorageError> {
        let existing = self
            .ctx
            .feed_source_repo
            .find_by_keys(&category.category.key, &source.key)
            .await?;
        let feed_source = if let Some(existing) = existing {
            existing
        } else {
            // F15-fix6：与 F15-fix3 同类——`feed_sources.config_version` 必须指
            // 向 `kind='config'` 的 rule_versions 行。原硬编码 `config_version: 1`
            // 在测试场景或首次部署下 id=1 可能根本不是 `kind='config'` 行（例如
            // 测试 fixture 先 INSERT 了 `kind='prompt'`），导致下游 active_rule_*
            // 反查不到对应 payload。改为 active_rule_or_register("config", ...)：
            // 生产路径走"读现有 active"，测试/首次部署走"seed placeholder"，
            // tag 显式标 `ingest-bootstrap` 让 admin 一眼看出是回退路径。
            //
            // 注：只在 bootstrap 分支（新建 FeedSource）触发该读路径，存量
            // FeedSource 复用既有 config_version 不变；对单次 ingest run 通常
            // 是 0~N 次（N=新增源数），不影响热路径性能。
            let config_version_id = self
                .ctx
                .rule_version_repo
                .active_rule_or_register(
                    "config",
                    "ingest-bootstrap",
                    "auto-registered by ingest when no active config rule existed",
                    "ingest-bootstrap",
                )
                .await?;
            let now = OffsetDateTime::now_utc();
            let new_source = FeedSource {
                id: 0,
                category_key: category.category.key.clone(),
                source_key: source.key.clone(),
                display_name: source.display_name.clone(),
                feed_url: source.feed_url.clone(),
                feed_kind: source.feed_kind,
                status: FeedSourceStatus::Active,
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
            let id = self.ctx.feed_source_repo.upsert(&new_source).await?;
            self.ctx
                .feed_source_repo
                .find_by_id(id)
                .await?
                .expect("upserted feed source should be readable")
        };

        Ok(SourceTask {
            category_key: feed_source.category_key,
            source_key: feed_source.source_key,
            source_id: feed_source.id,
            existing_etag: feed_source.etag,
            existing_last_modified: feed_source.last_modified,
            feed_url: feed_source.feed_url,
            feed_kind: feed_source.feed_kind,
        })
    }

    async fn process_source(ctx: Arc<RunContext>, task: SourceTask) -> IngestSourceOutcome {
        let now = OffsetDateTime::now_utc();
        let emitter = RunEventEmitter {
            run_id: &ctx.run_id,
            stage: "ingest",
            repo: ctx.event_repo.as_ref(),
        };
        let mut outcome = IngestSourceOutcome {
            source_id: task.source_id,
            category_key: task.category_key.clone(),
            source_key: task.source_key.clone(),
            status: IngestSourceStatus::Failed,
            entries_discovered: 0,
            entries_inserted: 0,
            entries_uid_dup: 0,
            entries_link_dup: 0,
            error_kind: None,
        };

        let fetch_request = FeedFetchRequest {
            source_id: task.source_id,
            category_key: task.category_key,
            source_key: task.source_key,
            feed_url: task.feed_url,
            feed_kind: task.feed_kind,
            etag: task.existing_etag,
            last_modified: task.existing_last_modified,
            timeout: StdDuration::from_secs(ctx.app.http.timeout_seconds),
        };

        let raw = match ctx.feed_fetcher.fetch_raw(&fetch_request).await {
            Ok(raw) => raw,
            Err(error) => {
                let message = error.display_user();
                emitter
                    .emit(
                        "source_fetch_failed",
                        "warn",
                        Some("feed_source"),
                        Some(task.source_id),
                        &message,
                        Some(json!({ "error_kind": error.error_kind() })),
                    )
                    .await;
                if let Err(storage_error) = ctx
                    .feed_source_repo
                    .update_after_fetch_failure(task.source_id, now, &message, error.error_kind())
                    .await
                {
                    tracing::warn!(
                        source_id = task.source_id,
                        "failed to update source failure: {storage_error}"
                    );
                }
                outcome.error_kind = Some(error.error_kind().to_string());
                return outcome;
            }
        };

        if raw.not_modified {
            if let Err(error) = ctx
                .feed_source_repo
                .update_after_fetch_success(
                    task.source_id,
                    raw.etag.as_deref(),
                    raw.last_modified.as_deref(),
                    now,
                    now,
                )
                .await
            {
                tracing::warn!(
                    source_id = task.source_id,
                    "failed to update source success: {error}"
                );
            }
            outcome.status = IngestSourceStatus::NotModified;
            return outcome;
        }

        let Some(raw_payload_bytes) = raw.raw_payload_bytes else {
            let message = "feed fetch returned no payload for non-304 response";
            emitter
                .emit(
                    "source_fetch_failed",
                    "warn",
                    Some("feed_source"),
                    Some(task.source_id),
                    message,
                    Some(json!({ "error_kind": "missing_payload" })),
                )
                .await;
            if let Err(error) = ctx
                .feed_source_repo
                .update_after_fetch_failure(task.source_id, now, message, "missing_payload")
                .await
            {
                tracing::warn!(
                    source_id = task.source_id,
                    "failed to update source failure: {error}"
                );
            }
            outcome.error_kind = Some("missing_payload".to_string());
            return outcome;
        };

        let artifact_writer = ArtifactWriter {
            config: &ctx.app.artifact,
            repo: ctx.artifact_repo.as_ref(),
        };
        if artifact_writer.should_write(false) {
            let artifact_key = task.source_id.to_string();
            if let Err(error) = artifact_writer
                .write_inline("feed_payload", &artifact_key, &raw_payload_bytes)
                .await
            {
                tracing::warn!(
                    source_id = task.source_id,
                    "failed to persist feed artifact: {error}"
                );
            }
        }

        let entries = match parse_feed(&raw_payload_bytes, task.feed_kind) {
            Ok(entries) => entries,
            Err(error) => {
                let message = error.display_user();
                emitter
                    .emit(
                        "source_fetch_failed",
                        "warn",
                        Some("feed_source"),
                        Some(task.source_id),
                        &message,
                        Some(json!({ "error_kind": error.error_kind() })),
                    )
                    .await;
                if let Err(storage_error) = ctx
                    .feed_source_repo
                    .update_after_fetch_failure(task.source_id, now, &message, error.error_kind())
                    .await
                {
                    tracing::warn!(
                        source_id = task.source_id,
                        "failed to update source failure: {storage_error}"
                    );
                }
                outcome.error_kind = Some(error.error_kind().to_string());
                return outcome;
            }
        };

        process_entries(&ctx, &emitter, &mut outcome, task.source_id, now, entries).await;

        if let Err(error) = ctx
            .feed_source_repo
            .update_after_fetch_success(
                task.source_id,
                raw.etag.as_deref(),
                raw.last_modified.as_deref(),
                now,
                now,
            )
            .await
        {
            tracing::warn!(
                source_id = task.source_id,
                "failed to update source success: {error}"
            );
        }

        outcome.status = IngestSourceStatus::Succeeded;
        outcome
    }
}

async fn process_entries(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    outcome: &mut IngestSourceOutcome,
    source_id: i64,
    now: OffsetDateTime,
    entries: Vec<FeedEntryMeta>,
) {
    let mut uid_dup_hits = Vec::new();
    let mut link_dup_hits = Vec::new();

    for meta in entries {
        outcome.entries_discovered += 1;
        let normalized = match normalize_link(&meta.link_raw) {
            Ok(normalized) => normalized,
            Err(error) => {
                tracing::debug!(
                    source_id,
                    feed_entry_uid = %meta.feed_entry_uid,
                    "skip entry with invalid link: {error}"
                );
                continue;
            }
        };

        match ctx
            .feed_entry_repo
            .exists_by_link_hash(&normalized.link_hash)
            .await
        {
            Ok(true) => {
                outcome.entries_link_dup += 1;
                link_dup_hits.push(json!({
                    "feed_entry_uid": meta.feed_entry_uid,
                    "link_hash": normalized.link_hash,
                    "normalized_link": normalized.normalized,
                }));
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    source_id,
                    feed_entry_uid = %meta.feed_entry_uid,
                    "link dedup lookup failed: {error}"
                );
                continue;
            }
        }

        let new_entry = NewFeedEntry {
            source_id,
            feed_entry_uid: meta.feed_entry_uid.clone(),
            normalized_link: normalized.normalized,
            link_hash: normalized.link_hash,
            title_raw: meta.title_raw,
            summary_raw: meta.summary_raw,
            published_at: meta.published_at,
            discovered_at: now,
        };

        match ctx.feed_entry_repo.insert_if_new(&new_entry).await {
            Ok(Some(entry_id)) => {
                outcome.entries_inserted += 1;
                tracing::info!(
                    target = "feed_entry.transition",
                    id = entry_id,
                    from = "none",
                    to = "pending_fetch",
                    reason = "ingest_discovered"
                );
            }
            Ok(None) => {
                outcome.entries_uid_dup += 1;
                uid_dup_hits.push(json!({ "feed_entry_uid": new_entry.feed_entry_uid }));
            }
            Err(error) => {
                tracing::warn!(
                    source_id,
                    feed_entry_uid = %new_entry.feed_entry_uid,
                    "feed entry insert failed: {error}"
                );
            }
        }
    }

    if !uid_dup_hits.is_empty() || !link_dup_hits.is_empty() {
        emitter
            .emit(
                "entry_dedup_skipped",
                "info",
                Some("feed_entry"),
                None,
                "feed entries skipped by ingest dedup",
                Some(json!({
                    "uid_dup": uid_dup_hits,
                    "link_dup": link_dup_hits,
                })),
            )
            .await;
    }
}

fn recalculate_summary(summary: &mut IngestSummary) {
    summary.sources_succeeded = 0;
    summary.sources_not_modified = 0;
    summary.sources_failed = 0;
    summary.entries_discovered = 0;
    summary.entries_inserted = 0;
    summary.entries_uid_dup = 0;
    summary.entries_link_dup = 0;

    for outcome in &summary.per_source {
        match outcome.status {
            IngestSourceStatus::Succeeded => summary.sources_succeeded += 1,
            IngestSourceStatus::NotModified => summary.sources_not_modified += 1,
            IngestSourceStatus::Failed => summary.sources_failed += 1,
        }
        summary.entries_discovered += outcome.entries_discovered;
        summary.entries_inserted += outcome.entries_inserted;
        summary.entries_uid_dup += outcome.entries_uid_dup;
        summary.entries_link_dup += outcome.entries_link_dup;
    }
}
