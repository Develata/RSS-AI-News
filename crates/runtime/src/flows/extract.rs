use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use rss_ai_news_domain::dto::extract::{ArticleFetchTask, ExtractedArticle, FallbackArticle};
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_domain::state::{ContentQuality, ExtractorStrategy};
use rss_ai_news_extractor::{ExtractorError, RawHtmlFetch, decode_html, summary_fallback};
use rss_ai_news_storage::{
    ArticleInsertOutcome, ClaimRequest, ClaimedFeedEntry, NewArticle, StorageError, build_owner_id,
    lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::task::JoinSet;

use crate::artifact::ArtifactWriter;
use crate::context::ExtractDeps;
use crate::events::RunEventEmitter;
use crate::flows::maintenance::emit_maintenance_outcome;

#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    pub batch_size: u32,
    pub max_attempts: u32,
    /// 单次 run 内部 claim 循环上限。`0` = 不限（仅由 lease + 宿主超时兜底）。
    /// 由 CLI 从 `app.runtime.max_batches_per_run` 传入（F6-3）。
    /// 详见 docs-backup/design/config-schema.md §4.4。
    pub max_batches: u32,
    /// Only claim entries of this category's sources (`ingest --category`,
    /// plan/09 §2); `None` = every category. Entries of non-active sources are
    /// never claimed.
    pub category_key: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ExtractSummary {
    pub claimed: u32,
    pub persisted: u32,
    pub fallback_persisted: u32,
    pub dedup_skipped: u32,
    pub retryable_failed: u32,
    pub permanent_failed: u32,
    /// Releases whose owner guard matched no row: another run reclaimed the
    /// expired lease, so this run changed nothing (the row is that run's).
    pub lease_lost: u32,
    /// Releases that failed with a storage error; the row keeps its lease
    /// until expiry reclaim returns it to `pending_fetch`.
    pub release_failed: u32,
    /// Entries whose outcome involved our own storage failing (article write,
    /// summary read or the release itself), whatever the final status. The
    /// CLI reports them as run failures, like `sources_storage_failed`.
    pub storage_failed: u32,
    /// Tasks that panicked or were cancelled; leases recover on expiry.
    pub tasks_panicked: u32,
    /// 实际执行的批次数（F6-3）。命中 `max_batches` 上限时等于上限值；
    /// 队列耗尽时小于上限。供 observability / 测试可见。
    pub batches_executed: u32,
    /// `true` 表示循环因 `max_batches` 上限退出（可能仍有可领取行）；
    /// `false` 表示自然耗尽（claim 返回空批次：没有未冷却的 pending 行）。
    /// 配合一条 INFO 日志（cli-semantics §4.4 line 198）让运维知晓本次 run
    /// 是否需要后续调度兜底。
    pub max_batches_reached: bool,
    /// `true` 表示本 run 至少有一行 retryable 失败且其 release 已落库（带 cooldown
    /// 回到 `pending_fetch`）；本 run 不再尝试这些行，重试推迟到之后的 run。
    /// release 落空计入 `lease_lost`，不计入此处。
    /// 它不终止循环，因此可与 `max_batches_reached` 同时为 `true`。
    pub retryable_deferred: bool,
    /// `error_kind` when claiming a batch failed; the loop stopped there.
    pub claim_error: Option<String>,
    pub failure_samples: Vec<ExtractEntryOutcome>,
}

#[derive(Debug, Clone)]
pub struct ExtractEntryOutcome {
    pub feed_entry_id: i64,
    pub status: ExtractEntryStatus,
    pub article_id: Option<i64>,
    pub error_kind: Option<String>,
    /// Our own storage failed on this entry's path (see
    /// [`ExtractSummary::storage_failed`]).
    pub storage_failed: bool,
}

impl ExtractEntryOutcome {
    fn new(
        feed_entry_id: i64,
        status: ExtractEntryStatus,
        article_id: Option<i64>,
        error_kind: Option<String>,
    ) -> Self {
        Self {
            feed_entry_id,
            status,
            article_id,
            error_kind,
            storage_failed: false,
        }
    }

    /// The owner-guarded release matched no row.
    fn lease_lost(feed_entry_id: i64, article_id: Option<i64>) -> Self {
        Self::new(
            feed_entry_id,
            ExtractEntryStatus::LeaseLost,
            article_id,
            Some("lease_conflict".to_string()),
        )
    }

    /// The release itself failed; nothing is known to have landed.
    fn release_failed(feed_entry_id: i64, article_id: Option<i64>, error: &StorageError) -> Self {
        Self {
            storage_failed: true,
            ..Self::new(
                feed_entry_id,
                ExtractEntryStatus::ReleaseFailed,
                article_id,
                Some(error.error_kind().to_string()),
            )
        }
    }
}

/// Outcome of one claimed entry. Every status except `LeaseLost` and
/// `ReleaseFailed` is reported only after its owner-guarded release returned
/// `Ok(true)`, i.e. the transition it names has landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractEntryStatus {
    Persisted,
    FallbackPersisted,
    DedupSkipped,
    RetryableFailed,
    PermanentFailed,
    /// Release matched no row: the lease was reclaimed by another run.
    LeaseLost,
    /// Release failed with a storage error; the lease expires and is reclaimed.
    ReleaseFailed,
}

pub struct ExtractFlow {
    ctx: Arc<ExtractDeps>,
}

impl ExtractFlow {
    pub fn new(ctx: Arc<ExtractDeps>) -> Self {
        Self { ctx }
    }

    pub async fn run(&self, opts: ExtractOptions) -> ExtractSummary {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run.run_id,
            stage: "extract",
            repo: self.ctx.event_repo.as_ref(),
        };
        emitter
            .emit(
                "run_started",
                "info",
                None,
                None,
                "extract run started",
                None,
            )
            .await;

        let owner = build_owner_id();
        let mut summary = ExtractSummary::default();
        // Ids claimed in this run whose outcome is not a confirmed terminal
        // success (failed, release missed, task panicked). Later claims exclude
        // them, so this run never retries an entry it already tried, even when
        // the release did not land (another run reclaimed the expired lease) or
        // a backfill reset the row. Confirmed successes leave the set: their
        // rows are terminal and can never be claimed again, so the list stays
        // proportional to this run's failures rather than to its total work.
        let mut excluded: HashSet<i64> = HashSet::new();
        // W15 §3.3：claim 过滤与 release 折叠必须用同一预算值，hoist 一处计算。
        let max_attempts = opts
            .max_attempts
            .max(self.ctx.retry.feed_entry_max_attempts);

        // W15 §5：首次 claim 前执行一次 ① reclaim + ② sweep（顺序固定，best-effort）。
        let maintenance_now = (self.ctx.clock)();
        let reclaimed = self
            .ctx
            .feed_entry_repo
            .reclaim_expired_leases(maintenance_now)
            .await;
        let swept = self
            .ctx
            .feed_entry_repo
            .terminalize_exhausted(max_attempts, maintenance_now)
            .await;
        emit_maintenance_outcome(&emitter, "feed_entries", reclaimed, Some(swept)).await;

        // F6-3: 0 表示不限，仅由 lease + 宿主超时兜底（config-schema §4.4 line 196）。
        // 内部用 Option<u32> 表达"无上限"；命中上限时主动 break + 写 INFO。
        let cap: Option<u32> = if opts.max_batches == 0 {
            None
        } else {
            Some(opts.max_batches)
        };

        loop {
            if cap.is_some_and(|c| summary.batches_executed >= c) {
                summary.max_batches_reached = true;
                tracing::info!(
                    stage = "extract",
                    batch_size = opts.batch_size,
                    max_batches = opts.max_batches,
                    batches_executed = summary.batches_executed,
                    "max_batches_per_run reached; remaining pending entries will be picked up by next run"
                );
                break;
            }

            let now = (self.ctx.clock)();
            let request = ClaimRequest {
                owner: owner.clone(),
                now,
                lease_expires_at: lease_expires_at(
                    now,
                    Duration::seconds(self.ctx.lease.fetch_duration_seconds as i64),
                ),
                batch_size: opts.batch_size.max(1),
                max_attempts,
            };
            let claimed = match self
                .ctx
                .feed_entry_repo
                .claim_pending_fetch_in_scope(
                    &request,
                    &excluded.iter().copied().collect::<Vec<_>>(),
                    opts.category_key.as_deref(),
                )
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    // Stop here; the completion event below still reports what
                    // earlier batches did.
                    tracing::error!("failed to claim extract entries: {error}");
                    summary.claim_error = Some(error.error_kind().to_string());
                    break;
                }
            };

            // 自然耗尽：本批次 claim 到空集，没有未冷却的 pending 行。
            if claimed.is_empty() {
                break;
            }

            summary.claimed += claimed.len() as u32;
            summary.batches_executed += 1;
            excluded.extend(claimed.iter().map(|entry| entry.id));

            let concurrency = self.ctx.http.concurrent_fetches.max(1) as usize;
            let mut join_set = JoinSet::new();
            let mut pending = claimed.into_iter();
            loop {
                // Only materialize task state for the next concurrency window.
                while join_set.len() < concurrency {
                    let Some(entry) = pending.next() else { break };
                    let ctx = Arc::clone(&self.ctx);
                    let owner = owner.clone();
                    join_set.spawn(Self::process_entry(ctx, owner, entry, max_attempts));
                }
                let Some(result) = join_set.join_next().await else {
                    break;
                };
                match result {
                    Ok(outcome) => {
                        if outcome.status.is_terminal_success() {
                            excluded.remove(&outcome.feed_entry_id);
                        }
                        summary.record(outcome);
                    }
                    Err(error) => {
                        tracing::error!("extract entry task panicked or was cancelled: {error}");
                        summary.tasks_panicked += 1;
                    }
                }
            }
            // Retryable failures do not stop the loop: their rows are cooling
            // down and excluded, so the next claim only takes other claimable
            // work.
        }
        if summary.retryable_deferred {
            tracing::info!(
                stage = "extract",
                retryable_failed = summary.retryable_failed,
                batches_executed = summary.batches_executed,
                "retryable failures; retries deferred to later runs"
            );
        }

        emitter
            .emit(
                "run_completed",
                "info",
                None,
                None,
                "extract run completed",
                Some(json!({
                    "claimed": summary.claimed,
                    "persisted": summary.persisted,
                    "fallback_persisted": summary.fallback_persisted,
                    "dedup_skipped": summary.dedup_skipped,
                    "retryable_failed": summary.retryable_failed,
                    "permanent_failed": summary.permanent_failed,
                    "lease_lost": summary.lease_lost,
                    "release_failed": summary.release_failed,
                    "storage_failed": summary.storage_failed,
                    "tasks_panicked": summary.tasks_panicked,
                    "batches_executed": summary.batches_executed,
                    "max_batches_reached": summary.max_batches_reached,
                    "retryable_deferred": summary.retryable_deferred,
                    "claim_error": summary.claim_error,
                })),
            )
            .await;

        summary
    }

    async fn process_entry(
        ctx: Arc<ExtractDeps>,
        owner: String,
        claimed: ClaimedFeedEntry,
        max_attempts: u32,
    ) -> ExtractEntryOutcome {
        let emitter = RunEventEmitter {
            run_id: &ctx.run.run_id,
            stage: "extract",
            repo: ctx.event_repo.as_ref(),
        };
        let entry = EntryRelease {
            ctx: &ctx,
            emitter: &emitter,
            owner: &owner,
            feed_entry_id: claimed.id,
            retry: RetrySchedule {
                max_attempts,
                attempt_count: claimed.attempt_count,
            },
        };
        let fetch_task = ArticleFetchTask {
            feed_entry_id: claimed.id,
            normalized_link: claimed.normalized_link,
            title_raw: claimed.title_raw,
            summary_raw: None,
            timeout: StdDuration::from_secs(ctx.http.timeout_seconds),
        };

        let raw = match ctx.html_fetcher.fetch_html(&fetch_task).await {
            Ok(raw) => raw,
            Err(error) => {
                let now = (ctx.clock)();
                // W18（plan/17）：永久性抓取失败（403 付费墙 / 404 / too_large）
                // 且 feed 摘要可用时降级成文，与下方解析链失败分支对称。
                // 可重试错误（超时/5xx）不消费摘要——重试仍有机会拿到全文，
                // W15 预算路径维持不变。
                if !error.is_retryable() {
                    match load_summary_fallback(&ctx, &fetch_task).await {
                        Ok(Some(fallback)) => {
                            tracing::info!(
                                feed_entry_id = claimed.id,
                                error_kind = error.error_kind(),
                                "permanent fetch failure; falling back to feed summary"
                            );
                            return entry.persist_fallback(fallback, None, now).await;
                        }
                        Ok(None) => {}
                        // Reading the summary failed: our storage, not the page,
                        // decides the release (a transient error is retried).
                        Err(storage_error) => {
                            return entry.release_storage_failure(&storage_error, now).await;
                        }
                    }
                }
                return entry.release_failure(&error, now).await;
            }
        };

        let now = (ctx.clock)();
        let artifact_writer = ArtifactWriter {
            config: &ctx.artifact,
            repo: ctx.artifact_repo.as_ref(),
        };
        let mut html_artifact_id = None;
        if !artifact_writer.waits_for_outcome() && artifact_writer.should_write(false) {
            html_artifact_id = write_html_artifact(&artifact_writer, claimed.id, &raw).await;
        }
        let chain = run_strategy_chain(&ctx, &fetch_task, &raw);
        // `on_failure` keeps the HTML only when no strategy extracts the article
        // (retryable error, summary fallback or permanent failure).
        if artifact_writer.waits_for_outcome() && !matches!(chain, ChainResult::Extracted(_)) {
            html_artifact_id = write_html_artifact(&artifact_writer, claimed.id, &raw).await;
        }
        match chain {
            ChainResult::Extracted(article) => {
                entry
                    .persist_extracted(article, html_artifact_id, now)
                    .await
            }
            ChainResult::Retryable(error) => entry.release_failure(&error, now).await,
            ChainResult::Failed(errors) => match load_summary_fallback(&ctx, &fetch_task).await {
                Ok(Some(fallback)) => {
                    entry
                        .persist_fallback(fallback, html_artifact_id, now)
                        .await
                }
                Ok(None) => {
                    let message = errors
                        .last()
                        .map(ClassifiedError::display_user)
                        .unwrap_or_else(|| "no extractor strategy succeeded".to_string());
                    entry.release_permanent(&message, "html_parse", now).await
                }
                Err(storage_error) => entry.release_storage_failure(&storage_error, now).await,
            },
        }
    }
}

/// Reads `summary_raw` lazily (only after a permanent fetch or parse failure).
/// `Ok(None)`: the entry has no usable summary. `Err`: the read itself failed;
/// the caller classifies it like any other storage error instead of treating
/// it as "no summary" (which would terminalize the entry on a transient blip).
async fn load_summary_fallback(
    ctx: &ExtractDeps,
    task: &ArticleFetchTask,
) -> Result<Option<FallbackArticle>, StorageError> {
    match ctx.feed_entry_repo.find_by_id(task.feed_entry_id).await {
        Ok(Some(entry)) => {
            let mut fallback_task = task.clone();
            fallback_task.summary_raw = entry.summary_raw;
            Ok(summary_fallback(&fallback_task))
        }
        Ok(None) => Ok(None),
        Err(error) => {
            tracing::warn!(
                feed_entry_id = task.feed_entry_id,
                "failed to load fallback summary: {error}"
            );
            Err(error)
        }
    }
}

enum ChainResult {
    Extracted(ExtractedArticle),
    Retryable(ExtractorError),
    Failed(Vec<ExtractorError>),
}

fn run_strategy_chain(
    ctx: &ExtractDeps,
    task: &ArticleFetchTask,
    raw: &RawHtmlFetch,
) -> ChainResult {
    // Strategies receive UTF-8: the charset comes from the BOM, the response
    // Content-Type or the page's <meta> (plan/02 §2.2). The html artifact keeps
    // the original bytes. An undecodable page is a permanent parse failure.
    let html = match decode_html(&raw.body_bytes, raw.content_type.as_deref()) {
        Ok(html) => html,
        Err(error) => return ChainResult::Failed(vec![error]),
    };
    let mut errors = Vec::new();
    for strategy in &ctx.strategies {
        match strategy.extract(task, html.as_bytes(), &raw.final_url) {
            Ok(article) if article.body_text.chars().count() >= ctx.min_body_chars as usize => {
                return ChainResult::Extracted(article);
            }
            Ok(_) => continue,
            Err(error) if error.is_retryable() => return ChainResult::Retryable(error),
            Err(error) => errors.push(error),
        }
    }
    ChainResult::Failed(errors)
}

async fn write_html_artifact(
    artifact_writer: &ArtifactWriter<'_>,
    feed_entry_id: i64,
    raw: &RawHtmlFetch,
) -> Option<i64> {
    match artifact_writer
        .write_inline("html_payload", &feed_entry_id.to_string(), &raw.body_bytes)
        .await
    {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(feed_entry_id, "failed to persist html artifact: {error}");
            None
        }
    }
}

/// Everything one claimed entry's release needs. Borrowed for the duration
/// of `process_entry`; one shared lifetime suffices because the emitter and
/// the owner string both outlive every call made through this struct.
struct EntryRelease<'a> {
    ctx: &'a ExtractDeps,
    emitter: &'a RunEventEmitter<'a>,
    owner: &'a str,
    feed_entry_id: i64,
    retry: RetrySchedule,
}

impl EntryRelease<'_> {
    async fn persist_extracted(
        &self,
        article: ExtractedArticle,
        html_artifact_id: Option<i64>,
        now: OffsetDateTime,
    ) -> ExtractEntryOutcome {
        let new_article = NewArticle {
            content_hash: article.content_hash,
            canonical_link: article.canonical_link,
            title: article.title,
            body_text: article.body_text,
            body_html_artifact_id: html_artifact_id,
            extractor_strategy: strategy_to_str(article.extractor_strategy).to_string(),
            extractor_version: 1,
            content_quality: quality_to_str(article.content_quality).to_string(),
            word_count: i64::from(article.word_count),
            origin_feed_entry_id: self.feed_entry_id,
        };
        self.persist_article(new_article, false, now).await
    }

    async fn persist_fallback(
        &self,
        fallback: FallbackArticle,
        html_artifact_id: Option<i64>,
        now: OffsetDateTime,
    ) -> ExtractEntryOutcome {
        let new_article = NewArticle {
            content_hash: fallback.content_hash,
            canonical_link: fallback.canonical_link,
            title: fallback.title,
            body_text: fallback.body_text,
            body_html_artifact_id: html_artifact_id,
            extractor_strategy: strategy_to_str(ExtractorStrategy::SummaryFallback).to_string(),
            extractor_version: 1,
            content_quality: quality_to_str(ContentQuality::Fallback).to_string(),
            word_count: i64::from(fallback.word_count),
            origin_feed_entry_id: self.feed_entry_id,
        };
        self.persist_article(new_article, true, now).await
    }

    async fn persist_article(
        &self,
        article: NewArticle,
        fallback_path: bool,
        now: OffsetDateTime,
    ) -> ExtractEntryOutcome {
        let ctx = self.ctx;
        let feed_entry_id = self.feed_entry_id;
        let insert = match ctx
            .article_repo
            .insert_or_get_by_content_hash(&article)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(feed_entry_id, "failed to persist article: {error}");
                // plan/11 §3：retryable storage errors take the budgeted
                // cooldown release, keeping the real error kind.
                return self.release_storage_failure(&error, now).await;
            }
        };

        if !insert.newly_created {
            return self.release_dedup_skipped(insert, now).await;
        }

        let released = if fallback_path {
            ctx.feed_entry_repo
                .release_fallback_persisted(feed_entry_id, self.owner, insert.article_id, now)
                .await
        } else {
            ctx.feed_entry_repo
                .release_success(feed_entry_id, self.owner, insert.article_id, now)
                .await
        };

        match released {
            Ok(true) => {
                let (to, reason, status) = if fallback_path {
                    (
                        "fallback_persisted",
                        "summary_fallback",
                        ExtractEntryStatus::FallbackPersisted,
                    )
                } else {
                    (
                        "persisted",
                        "extract_success",
                        ExtractEntryStatus::Persisted,
                    )
                };
                tracing::info!(
                    target = "feed_entry.transition",
                    id = feed_entry_id,
                    from = "fetching",
                    to,
                    reason
                );
                ExtractEntryOutcome::new(feed_entry_id, status, Some(insert.article_id), None)
            }
            Ok(false) => {
                tracing::warn!(feed_entry_id, "feed entry release conflicted");
                ExtractEntryOutcome::lease_lost(feed_entry_id, Some(insert.article_id))
            }
            Err(error) => {
                tracing::error!(feed_entry_id, "feed entry release failed: {error}");
                ExtractEntryOutcome::release_failed(feed_entry_id, Some(insert.article_id), &error)
            }
        }
    }

    async fn release_dedup_skipped(
        &self,
        insert: ArticleInsertOutcome,
        now: OffsetDateTime,
    ) -> ExtractEntryOutcome {
        let feed_entry_id = self.feed_entry_id;
        let released = self
            .ctx
            .feed_entry_repo
            .release_dedup_skipped(
                feed_entry_id,
                self.owner,
                insert.article_id,
                "hash_dup",
                now,
            )
            .await;
        match released {
            Ok(true) => {
                self.emitter
                    .emit(
                        "entry_dedup_skipped",
                        "info",
                        Some("feed_entry"),
                        Some(feed_entry_id),
                        "feed entry skipped by content hash dedup",
                        Some(json!({
                            "existing_article_id": insert.article_id,
                            "decision": "hash_dup",
                        })),
                    )
                    .await;
                tracing::info!(
                    target = "feed_entry.transition",
                    id = feed_entry_id,
                    from = "fetching",
                    to = "dedup_skipped",
                    reason = "hash_dup"
                );
                ExtractEntryOutcome::new(
                    feed_entry_id,
                    ExtractEntryStatus::DedupSkipped,
                    Some(insert.article_id),
                    None,
                )
            }
            Ok(false) => {
                tracing::warn!(feed_entry_id, "dedup release conflicted");
                ExtractEntryOutcome::lease_lost(feed_entry_id, Some(insert.article_id))
            }
            Err(error) => {
                tracing::error!(feed_entry_id, "dedup release failed: {error}");
                ExtractEntryOutcome::release_failed(feed_entry_id, Some(insert.article_id), &error)
            }
        }
    }

    /// [`Self::release_failure`] for a failure of our own storage: the outcome
    /// is flagged `storage_failed` whatever release it ends in.
    async fn release_storage_failure(
        &self,
        error: &StorageError,
        now: OffsetDateTime,
    ) -> ExtractEntryOutcome {
        ExtractEntryOutcome {
            storage_failed: true,
            ..self.release_failure(error, now).await
        }
    }

    /// Classified release: retryable errors (network, 5xx, transient storage)
    /// return the row to `pending_fetch` with a cooldown inside the retry
    /// budget (W15 folds the last attempt into `failed`); others are permanent.
    /// `last_error*` always carry the real error.
    async fn release_failure(
        &self,
        error: &(dyn ClassifiedError + Sync),
        now: OffsetDateTime,
    ) -> ExtractEntryOutcome {
        let message = error.display_user();
        let kind = error.error_kind().to_string();
        if !error.is_retryable() {
            return self.release_permanent(&message, &kind, now).await;
        }
        let ctx = self.ctx;
        let feed_entry_id = self.feed_entry_id;
        // One instant for the row's `updated_at` and the cooldown: `now` may
        // predate artifact writes and the strategy chain, which would shorten
        // the cooldown and misdate the release.
        let released_at = (ctx.clock)();
        // W15 §3：release SQL 内按预算折叠——耗尽即转 failed 终态。
        let outcome = match ctx
            .feed_entry_repo
            .release_retryable_failure(
                feed_entry_id,
                self.owner,
                &message,
                &kind,
                self.retry.max_attempts,
                self.retry.next_attempt_at(released_at),
                released_at,
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(feed_entry_id, "retryable release failed: {error}");
                return ExtractEntryOutcome::release_failed(feed_entry_id, None, &error);
            }
        };
        if !outcome.released {
            tracing::warn!(feed_entry_id, "retryable release conflicted");
            return ExtractEntryOutcome::lease_lost(feed_entry_id, None);
        }
        if outcome.exhausted {
            self.emitter
                .emit(
                    "entry_permanent_failed",
                    "error",
                    Some("feed_entry"),
                    Some(feed_entry_id),
                    &message,
                    Some(json!({ "error_kind": kind, "budget_exhausted": true })),
                )
                .await;
            tracing::error!(
                target = "feed_entry.transition",
                id = feed_entry_id,
                from = "fetching",
                to = "failed",
                reason = %kind,
                budget_exhausted = true
            );
            return ExtractEntryOutcome::new(
                feed_entry_id,
                ExtractEntryStatus::PermanentFailed,
                None,
                Some(kind),
            );
        }
        tracing::warn!(
            feed_entry_id,
            error_kind = %kind,
            "extract retryable failure"
        );
        ExtractEntryOutcome::new(
            feed_entry_id,
            ExtractEntryStatus::RetryableFailed,
            None,
            Some(kind),
        )
    }

    /// Permanent release. The `entry_permanent_failed` event and transition
    /// log are written only when the release landed.
    async fn release_permanent(
        &self,
        message: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> ExtractEntryOutcome {
        let feed_entry_id = self.feed_entry_id;
        match self
            .ctx
            .feed_entry_repo
            .release_permanent_failure(feed_entry_id, self.owner, message, kind, now)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(feed_entry_id, "permanent release conflicted");
                return ExtractEntryOutcome::lease_lost(feed_entry_id, None);
            }
            Err(error) => {
                tracing::error!(feed_entry_id, "permanent release failed: {error}");
                return ExtractEntryOutcome::release_failed(feed_entry_id, None, &error);
            }
        }
        self.emitter
            .emit(
                "entry_permanent_failed",
                "error",
                Some("feed_entry"),
                Some(feed_entry_id),
                message,
                Some(json!({ "error_kind": kind })),
            )
            .await;
        tracing::error!(
            target = "feed_entry.transition",
            id = feed_entry_id,
            from = "fetching",
            to = "failed",
            reason = kind
        );
        ExtractEntryOutcome::new(
            feed_entry_id,
            ExtractEntryStatus::PermanentFailed,
            None,
            Some(kind.to_string()),
        )
    }
}
/// Shortest retry cooldown, applied after the first failed attempt.
const RETRY_BACKOFF_BASE: Duration = Duration::minutes(10);
/// Longest retry cooldown; later attempts stop doubling here.
const RETRY_BACKOFF_CAP: Duration = Duration::hours(2);

/// Retry budget and cooldown inputs for one claimed entry.
#[derive(Debug, Clone, Copy)]
struct RetrySchedule {
    max_attempts: u32,
    /// `attempt_count` after the claim's increment (the current attempt, ≥ 1).
    attempt_count: i64,
}

impl RetrySchedule {
    /// When a retryable failure released at `now` becomes claimable again.
    /// The releasing run itself never retries the entry (it stays excluded).
    fn next_attempt_at(&self, now: OffsetDateTime) -> OffsetDateTime {
        now + retry_backoff(self.attempt_count)
    }
}

/// Bounded exponential backoff: `min(BASE * 2^(attempt - 1), CAP)`.
///
/// `attempt` is the attempt that just failed (1 for the first); values below 1
/// are treated as 1. The default 5-attempt budget waits 10, 20, 40 and 80 min.
fn retry_backoff(attempt: i64) -> Duration {
    // 2^4 * BASE already exceeds CAP; clamping the exponent avoids overflow.
    let exponent = attempt.clamp(1, 5) - 1;
    (RETRY_BACKOFF_BASE * (1_i32 << exponent)).min(RETRY_BACKOFF_CAP)
}

fn strategy_to_str(strategy: ExtractorStrategy) -> &'static str {
    match strategy {
        ExtractorStrategy::Readability => "readability",
        ExtractorStrategy::SummaryFallback => "summary_fallback",
    }
}

fn quality_to_str(quality: ContentQuality) -> &'static str {
    match quality {
        ContentQuality::High => "high",
        ContentQuality::Medium => "medium",
        ContentQuality::Fallback => "fallback",
    }
}

impl ExtractEntryStatus {
    /// `true` only when the row's terminal release landed: these statuses are
    /// produced exclusively after `release_*` returned `Ok(true)`. Conflicts and
    /// release errors are reported as `LeaseLost` / `ReleaseFailed`.
    fn is_terminal_success(&self) -> bool {
        matches!(
            self,
            Self::Persisted | Self::FallbackPersisted | Self::DedupSkipped
        )
    }
}

impl ExtractSummary {
    /// Count every outcome; retain only the first 32 failures for diagnostics.
    fn record(&mut self, outcome: ExtractEntryOutcome) {
        match outcome.status {
            ExtractEntryStatus::Persisted => self.persisted += 1,
            ExtractEntryStatus::FallbackPersisted => self.fallback_persisted += 1,
            ExtractEntryStatus::DedupSkipped => self.dedup_skipped += 1,
            ExtractEntryStatus::RetryableFailed => {
                self.retryable_failed += 1;
                self.retryable_deferred = true;
            }
            ExtractEntryStatus::PermanentFailed => self.permanent_failed += 1,
            ExtractEntryStatus::LeaseLost => self.lease_lost += 1,
            ExtractEntryStatus::ReleaseFailed => self.release_failed += 1,
        }
        if outcome.storage_failed {
            self.storage_failed += 1;
        }
        if !outcome.status.is_terminal_success() && self.failure_samples.len() < 32 {
            self.failure_samples.push(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_doubles_and_is_bounded() {
        let minutes = |attempt| retry_backoff(attempt).whole_minutes();
        assert_eq!(minutes(i64::MIN), 10);
        assert_eq!(minutes(0), 10);
        assert_eq!(minutes(1), 10);
        assert_eq!(minutes(2), 20);
        assert_eq!(minutes(3), 40);
        assert_eq!(minutes(4), 80);
        assert_eq!(minutes(5), 120);
        assert_eq!(minutes(i64::MAX), 120);
    }

    #[test]
    fn next_attempt_adds_backoff_for_the_failed_attempt() {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let retry = |attempt_count| RetrySchedule {
            max_attempts: 5,
            attempt_count,
        };
        assert_eq!(retry(1).next_attempt_at(now), now + Duration::minutes(10));
        assert_eq!(retry(3).next_attempt_at(now), now + Duration::minutes(40));
    }

    #[test]
    fn only_landed_terminal_outcomes_leave_the_exclusion_set() {
        use ExtractEntryStatus::*;
        assert!(Persisted.is_terminal_success());
        assert!(FallbackPersisted.is_terminal_success());
        assert!(DedupSkipped.is_terminal_success());
        assert!(!RetryableFailed.is_terminal_success());
        assert!(!PermanentFailed.is_terminal_success());
        assert!(!LeaseLost.is_terminal_success());
        assert!(!ReleaseFailed.is_terminal_success());
    }

    #[test]
    fn summary_bounds_samples_without_losing_counts() {
        let mut summary = ExtractSummary {
            tasks_panicked: 4,
            ..Default::default()
        };
        for _ in 0..10_000 {
            summary.record(ExtractEntryOutcome {
                feed_entry_id: 1,
                article_id: None,
                status: ExtractEntryStatus::Persisted,
                error_kind: None,
                storage_failed: false,
            });
            summary.record(ExtractEntryOutcome {
                feed_entry_id: 1,
                article_id: None,
                status: ExtractEntryStatus::PermanentFailed,
                error_kind: Some("failure".into()),
                storage_failed: true,
            });
        }
        summary.record(ExtractEntryOutcome {
            feed_entry_id: 1,
            article_id: None,
            status: ExtractEntryStatus::RetryableFailed,
            error_kind: None,
            storage_failed: false,
        });
        summary.record(ExtractEntryOutcome::lease_lost(2, None));
        assert_eq!(summary.persisted, 10_000);
        assert_eq!(summary.storage_failed, 10_000);
        assert_eq!(summary.lease_lost, 1);
        assert_eq!(summary.permanent_failed, 10_000);
        assert_eq!(summary.retryable_failed, 1);
        assert_eq!(summary.failure_samples.len(), 32);
        assert_eq!(summary.tasks_panicked, 4);
    }
}
