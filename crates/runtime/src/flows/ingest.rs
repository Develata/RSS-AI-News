use std::sync::Arc;
use std::time::Duration as StdDuration;

use rss_ai_news_config::{CategoryConfig, SourceConfig, SourceSecrets};
use rss_ai_news_domain::SecretString;
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
    rsshub_access_key: Option<SecretString>,
}

pub struct IngestFlow {
    ctx: Arc<RunContext>,
    categories: Vec<CategoryConfig>,
    source_secrets: SourceSecrets,
}

impl IngestFlow {
    pub fn new(ctx: Arc<RunContext>, categories: Vec<CategoryConfig>) -> Self {
        Self::with_source_secrets(ctx, categories, SourceSecrets::default())
    }

    pub fn with_source_secrets(
        ctx: Arc<RunContext>,
        categories: Vec<CategoryConfig>,
        source_secrets: SourceSecrets,
    ) -> Self {
        Self {
            ctx,
            categories,
            source_secrets,
        }
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

    /// W16（docs/plan/16-config-versioning.md §6）：拿"当前生效 config 版本"
    /// 行 id。CLI 路径下启动期 seed 已保证 active 行跟随真实 sha，走读分支；
    /// 库内嵌/测试无 active 行时 seed placeholder（tag 显式标
    /// `ingest-bootstrap` 让 admin 一眼看出是回退路径），下次 CLI 启动被
    /// rotate 收编为 superseded。
    async fn active_config_version_id(&self) -> Result<i64, rss_ai_news_storage::StorageError> {
        self.ctx
            .rule_version_repo
            .active_rule_or_register(
                "config",
                "ingest-bootstrap",
                "auto-registered by ingest when no active config rule existed",
                "ingest-bootstrap",
            )
            .await
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
            let needs_sync = existing.display_name != source.display_name
                || existing.feed_url != source.feed_url
                || existing.feed_kind != source.feed_kind
                || existing.status != FeedSourceStatus::Active
                || existing.priority != i64::from(source.priority);
            if needs_sync {
                let endpoint_changed =
                    existing.feed_url != source.feed_url || existing.feed_kind != source.feed_kind;
                let mut updated = existing;
                updated.display_name = source.display_name.clone();
                updated.feed_url = source.feed_url.clone();
                updated.feed_kind = source.feed_kind;
                updated.status = FeedSourceStatus::Active;
                updated.priority = i64::from(source.priority);
                // W16 §6：行被配置变化触发重写时，版本戳必须跟随触发这次
                // 重写的 config（旧值是上一份 config 的事实，继续沿用会让
                // "哪个配置产生了这行"在审计上指错）。仅 needs_sync 时多
                // 一次 active_rule 读，不影响无变化热路径。
                updated.config_version = self.active_config_version_id().await?;
                updated.updated_at = OffsetDateTime::now_utc();
                if endpoint_changed {
                    updated.etag = None;
                    updated.last_modified = None;
                    updated.consecutive_failures = 0;
                    updated.last_error = None;
                    updated.last_error_kind = None;
                }
                let id = self.ctx.feed_source_repo.upsert(&updated).await?;
                self.ctx
                    .feed_source_repo
                    .find_by_id(id)
                    .await?
                    .expect("upserted feed source should be readable")
            } else {
                existing
            }
        } else {
            // F15-fix6：`feed_sources.config_version` 必须指向 `kind='config'`
            // 的 rule_versions 行（原硬编码 `1` 在测试/首次部署下可能指向
            // 其它 kind）。W16 起读路径收敛到 active_config_version_id helper，
            // 与 needs_sync 分支共用。
            let config_version_id = self.active_config_version_id().await?;
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
            rsshub_access_key: self
                .source_secrets
                .rsshub_access_key(&category.category.key, &source.key)
                .cloned(),
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
            rsshub_access_key: task.rsshub_access_key,
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
