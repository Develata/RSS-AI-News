use std::sync::Arc;
use std::time::Duration as StdDuration;

use rss_ai_news_domain::dto::extract::{ArticleFetchTask, ExtractedArticle, FallbackArticle};
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_domain::state::{ContentQuality, ExtractorStrategy};
use rss_ai_news_extractor::{ExtractorError, RawHtmlFetch, summary_fallback};
use rss_ai_news_storage::{
    ArticleInsertOutcome, ClaimRequest, ClaimedFeedEntry, NewArticle, build_owner_id,
    lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::artifact::ArtifactWriter;
use crate::context::RunContext;
use crate::events::RunEventEmitter;

#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    pub batch_size: u32,
    pub max_attempts: u32,
    /// 单次 run 内部 claim 循环上限。`0` = 不限（仅由 lease + 宿主超时兜底）。
    /// 由 CLI 从 `app.runtime.max_batches_per_run` 传入（F6-3）。
    /// 详见 docs/design/config-schema.md §4.4。
    pub max_batches: u32,
}

#[derive(Debug, Default, Clone)]
pub struct ExtractSummary {
    pub claimed: u32,
    pub persisted: u32,
    pub fallback_persisted: u32,
    pub dedup_skipped: u32,
    pub retryable_failed: u32,
    pub permanent_failed: u32,
    /// 实际执行的批次数（F6-3）。命中 `max_batches` 上限时等于上限值；
    /// 队列耗尽时小于上限。供 observability / 测试可见。
    pub batches_executed: u32,
    /// `true` 表示循环因 `max_batches` 上限退出（仍有 pending 行）；
    /// `false` 表示自然耗尽（claim 返回空批次）或因 retryable 失败 defer。
    /// 配合一条 INFO 日志（cli-semantics §4.4 line 198）让运维知晓本次 run
    /// 是否需要后续调度兜底。`max_batches_reached` 与 `retryable_deferred`
    /// 互斥（同一 run 内最多有一个为 `true`）。
    pub max_batches_reached: bool,
    /// `true` 表示循环因本批次出现 RetryableFailed 主动 defer 到下次 run
    /// （F6-3 retryable-bail 路径；W4-1）。`false` 表示既未 defer 也未命中
    /// 上限——配合 `max_batches_reached` 三值组合可区分三种退出路径：
    /// `(cap_hit, retryable, queue_exhausted)` → `(T, F)`, `(F, T)`, `(F, F)`。
    /// 让 observability 消费者无须靠 `batches_executed < cap` 隐式推断。
    pub retryable_deferred: bool,
    pub per_entry: Vec<ExtractEntryOutcome>,
}

#[derive(Debug, Clone)]
pub struct ExtractEntryOutcome {
    pub feed_entry_id: i64,
    pub status: ExtractEntryStatus,
    pub article_id: Option<i64>,
    pub error_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractEntryStatus {
    Persisted,
    FallbackPersisted,
    DedupSkipped,
    RetryableFailed,
    PermanentFailed,
}

pub struct ExtractFlow {
    ctx: Arc<RunContext>,
}

impl ExtractFlow {
    pub fn new(ctx: Arc<RunContext>) -> Self {
        Self { ctx }
    }

    pub async fn run(&self, opts: ExtractOptions) -> ExtractSummary {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
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

            let now = OffsetDateTime::now_utc();
            let request = ClaimRequest {
                owner: owner.clone(),
                now,
                lease_expires_at: lease_expires_at(
                    now,
                    Duration::seconds(self.ctx.app.lease.fetch_duration_seconds as i64),
                ),
                batch_size: opts.batch_size.max(1),
                max_attempts: opts
                    .max_attempts
                    .max(self.ctx.app.retry.feed_entry_max_attempts),
            };
            let claimed = match self.ctx.feed_entry_repo.claim_pending_fetch(&request).await {
                Ok(claimed) => claimed,
                Err(error) => {
                    tracing::error!("failed to claim extract entries: {error}");
                    emitter
                        .emit(
                            "run_completed",
                            "info",
                            None,
                            None,
                            "extract run completed",
                            Some(json!({
                                "claimed": summary.claimed,
                                "claim_error": error.error_kind(),
                                "batches_executed": summary.batches_executed,
                            })),
                        )
                        .await;
                    return summary;
                }
            };

            // 自然耗尽：本批次 claim 到空集，pending 队列已无可处理项。
            if claimed.is_empty() {
                break;
            }

            summary.claimed += claimed.len() as u32;
            summary.batches_executed += 1;
            let per_entry_len_before = summary.per_entry.len();

            let semaphore = Arc::new(Semaphore::new(
                self.ctx.app.http.concurrent_fetches.max(1) as usize,
            ));
            let mut join_set = JoinSet::new();

            for entry in claimed {
                let ctx = Arc::clone(&self.ctx);
                let owner = owner.clone();
                let semaphore = Arc::clone(&semaphore);
                join_set.spawn(async move {
                    let _permit = semaphore
                        .acquire_owned()
                        .await
                        .expect("semaphore should not be closed");
                    Self::process_entry(ctx, owner, entry).await
                });
            }

            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(outcome) => summary.per_entry.push(outcome),
                    Err(error) => {
                        tracing::error!("extract entry task panicked or was cancelled: {error}");
                        summary.permanent_failed += 1;
                    }
                }
            }

            // F6-3: 本批次产生了 retryable 失败 ⇒ 这些行已经回到 pending_fetch
            // 且 lease 释放，下一次 claim 会立即把它们捞回来形成 retry-loop。
            // 此处主动终止，留待下一次 run 重试（符合
            // `extract_releases_retryable_on_5xx` 测试以及"重试跨 run 而非
            // 同 run"的语义）。
            let batch_retryable = summary.per_entry[per_entry_len_before..]
                .iter()
                .filter(|o| matches!(o.status, ExtractEntryStatus::RetryableFailed))
                .count();
            if batch_retryable > 0 {
                summary.retryable_deferred = true;
                tracing::info!(
                    stage = "extract",
                    batches_executed = summary.batches_executed,
                    batch_retryable,
                    "retryable failures in batch; deferring re-claim to next run"
                );
                break;
            }
        }

        recalculate_summary(&mut summary);
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
                    "batches_executed": summary.batches_executed,
                    "max_batches_reached": summary.max_batches_reached,
                    "retryable_deferred": summary.retryable_deferred,
                })),
            )
            .await;

        summary
    }

    async fn process_entry(
        ctx: Arc<RunContext>,
        owner: String,
        claimed: ClaimedFeedEntry,
    ) -> ExtractEntryOutcome {
        let now = OffsetDateTime::now_utc();
        let emitter = RunEventEmitter {
            run_id: &ctx.run_id,
            stage: "extract",
            repo: ctx.event_repo.as_ref(),
        };
        let summary_raw = match ctx.feed_entry_repo.find_by_id(claimed.id).await {
            Ok(Some(entry)) => entry.summary_raw,
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    feed_entry_id = claimed.id,
                    "failed to load feed entry summary for fallback: {error}"
                );
                None
            }
        };
        let fetch_task = ArticleFetchTask {
            feed_entry_id: claimed.id,
            normalized_link: claimed.normalized_link,
            title_raw: claimed.title_raw,
            summary_raw,
            timeout: StdDuration::from_secs(ctx.app.http.timeout_seconds),
        };

        let raw = match ctx.html_fetcher.fetch_html(&fetch_task).await {
            Ok(raw) => raw,
            Err(error) => {
                return release_extract_error(&ctx, &emitter, claimed.id, &owner, error, now).await;
            }
        };

        let html_artifact_id = write_html_artifact(&ctx, claimed.id, &raw).await;
        match run_strategy_chain(&ctx, &fetch_task, &raw) {
            ChainResult::Extracted(article) => {
                persist_extracted(
                    &ctx,
                    &emitter,
                    &owner,
                    claimed.id,
                    article,
                    html_artifact_id,
                    now,
                )
                .await
            }
            ChainResult::Retryable(error) => {
                release_extract_error(&ctx, &emitter, claimed.id, &owner, error, now).await
            }
            ChainResult::Failed(errors) => {
                if let Some(fallback) = summary_fallback(&fetch_task) {
                    persist_fallback(
                        &ctx,
                        &emitter,
                        &owner,
                        claimed.id,
                        fallback,
                        html_artifact_id,
                        now,
                    )
                    .await
                } else {
                    let message = errors
                        .last()
                        .map(ClassifiedError::display_user)
                        .unwrap_or_else(|| "no extractor strategy succeeded".to_string());
                    release_permanent_failure(
                        &ctx,
                        &emitter,
                        claimed.id,
                        &owner,
                        &message,
                        "html_parse",
                        now,
                    )
                    .await
                }
            }
        }
    }
}

enum ChainResult {
    Extracted(ExtractedArticle),
    Retryable(ExtractorError),
    Failed(Vec<ExtractorError>),
}

fn run_strategy_chain(
    ctx: &RunContext,
    task: &ArticleFetchTask,
    raw: &RawHtmlFetch,
) -> ChainResult {
    let mut errors = Vec::new();
    for strategy in &ctx.strategies {
        match strategy.extract(task, &raw.body_bytes, &raw.final_url) {
            Ok(article) => return ChainResult::Extracted(article),
            Err(error) if error.is_retryable() => return ChainResult::Retryable(error),
            Err(error) => errors.push(error),
        }
    }
    ChainResult::Failed(errors)
}

async fn write_html_artifact(
    ctx: &RunContext,
    feed_entry_id: i64,
    raw: &RawHtmlFetch,
) -> Option<i64> {
    let artifact_writer = ArtifactWriter {
        config: &ctx.app.artifact,
        repo: ctx.artifact_repo.as_ref(),
    };
    if !artifact_writer.should_write(false) {
        return None;
    }
    match artifact_writer
        .write_inline("html_payload", &feed_entry_id.to_string(), &raw.body_bytes)
        .await
    {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(
                feed_entry_id,
                "failed to persist html artifact before extract: {error}"
            );
            None
        }
    }
}

async fn persist_extracted(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    feed_entry_id: i64,
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
        origin_feed_entry_id: feed_entry_id,
    };
    persist_article(ctx, emitter, owner, feed_entry_id, new_article, false, now).await
}

async fn persist_fallback(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    feed_entry_id: i64,
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
        origin_feed_entry_id: feed_entry_id,
    };
    persist_article(ctx, emitter, owner, feed_entry_id, new_article, true, now).await
}

async fn persist_article(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    feed_entry_id: i64,
    article: NewArticle,
    fallback_path: bool,
    now: OffsetDateTime,
) -> ExtractEntryOutcome {
    let insert = match ctx
        .article_repo
        .insert_or_get_by_content_hash(&article)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::error!(feed_entry_id, "failed to persist article: {error}");
            return release_permanent_failure(
                ctx,
                emitter,
                feed_entry_id,
                owner,
                &error.display_user(),
                error.error_kind(),
                now,
            )
            .await;
        }
    };

    if !insert.newly_created {
        return release_dedup_skipped(ctx, emitter, owner, feed_entry_id, insert, now).await;
    }

    let released = if fallback_path {
        ctx.feed_entry_repo
            .release_fallback_persisted(feed_entry_id, owner, insert.article_id, now)
            .await
    } else {
        ctx.feed_entry_repo
            .release_success(feed_entry_id, owner, insert.article_id, now)
            .await
    };

    match released {
        Ok(true) if fallback_path => {
            tracing::info!(
                target = "feed_entry.transition",
                id = feed_entry_id,
                from = "fetching",
                to = "fallback_persisted",
                reason = "summary_fallback"
            );
            ExtractEntryOutcome {
                feed_entry_id,
                status: ExtractEntryStatus::FallbackPersisted,
                article_id: Some(insert.article_id),
                error_kind: None,
            }
        }
        Ok(true) => {
            tracing::info!(
                target = "feed_entry.transition",
                id = feed_entry_id,
                from = "fetching",
                to = "persisted",
                reason = "extract_success"
            );
            ExtractEntryOutcome {
                feed_entry_id,
                status: ExtractEntryStatus::Persisted,
                article_id: Some(insert.article_id),
                error_kind: None,
            }
        }
        Ok(false) => {
            tracing::warn!(feed_entry_id, "feed entry release conflicted");
            ExtractEntryOutcome {
                feed_entry_id,
                status: ExtractEntryStatus::PermanentFailed,
                article_id: Some(insert.article_id),
                error_kind: Some("lease_conflict".to_string()),
            }
        }
        Err(error) => {
            tracing::error!(feed_entry_id, "feed entry release failed: {error}");
            ExtractEntryOutcome {
                feed_entry_id,
                status: ExtractEntryStatus::PermanentFailed,
                article_id: Some(insert.article_id),
                error_kind: Some(error.error_kind().to_string()),
            }
        }
    }
}

async fn release_dedup_skipped(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    owner: &str,
    feed_entry_id: i64,
    insert: ArticleInsertOutcome,
    now: OffsetDateTime,
) -> ExtractEntryOutcome {
    let released = ctx
        .feed_entry_repo
        .release_dedup_skipped(feed_entry_id, owner, insert.article_id, "hash_dup", now)
        .await;
    match released {
        Ok(true) => {
            emitter
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
            ExtractEntryOutcome {
                feed_entry_id,
                status: ExtractEntryStatus::DedupSkipped,
                article_id: Some(insert.article_id),
                error_kind: None,
            }
        }
        Ok(false) => ExtractEntryOutcome {
            feed_entry_id,
            status: ExtractEntryStatus::PermanentFailed,
            article_id: Some(insert.article_id),
            error_kind: Some("lease_conflict".to_string()),
        },
        Err(error) => ExtractEntryOutcome {
            feed_entry_id,
            status: ExtractEntryStatus::PermanentFailed,
            article_id: Some(insert.article_id),
            error_kind: Some(error.error_kind().to_string()),
        },
    }
}

async fn release_extract_error(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    feed_entry_id: i64,
    owner: &str,
    error: ExtractorError,
    now: OffsetDateTime,
) -> ExtractEntryOutcome {
    let message = error.display_user();
    let kind = error.error_kind().to_string();
    if error.is_retryable() {
        match ctx
            .feed_entry_repo
            .release_retryable_failure(feed_entry_id, owner, &message, &kind, now)
            .await
        {
            Ok(false) => tracing::warn!(feed_entry_id, "retryable release conflicted"),
            Err(error) => tracing::error!(feed_entry_id, "retryable release failed: {error}"),
            Ok(true) => {}
        }
        tracing::warn!(
            feed_entry_id,
            error_kind = %kind,
            "extract retryable failure"
        );
        ExtractEntryOutcome {
            feed_entry_id,
            status: ExtractEntryStatus::RetryableFailed,
            article_id: None,
            error_kind: Some(kind),
        }
    } else {
        release_permanent_failure(ctx, emitter, feed_entry_id, owner, &message, &kind, now).await
    }
}

async fn release_permanent_failure(
    ctx: &RunContext,
    emitter: &RunEventEmitter<'_>,
    feed_entry_id: i64,
    owner: &str,
    message: &str,
    kind: &str,
    now: OffsetDateTime,
) -> ExtractEntryOutcome {
    match ctx
        .feed_entry_repo
        .release_permanent_failure(feed_entry_id, owner, message, kind, now)
        .await
    {
        Ok(false) => tracing::warn!(feed_entry_id, "permanent release conflicted"),
        Err(error) => tracing::error!(feed_entry_id, "permanent release failed: {error}"),
        Ok(true) => {}
    }
    emitter
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
    ExtractEntryOutcome {
        feed_entry_id,
        status: ExtractEntryStatus::PermanentFailed,
        article_id: None,
        error_kind: Some(kind.to_string()),
    }
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

fn recalculate_summary(summary: &mut ExtractSummary) {
    summary.persisted = 0;
    summary.fallback_persisted = 0;
    summary.dedup_skipped = 0;
    summary.retryable_failed = 0;
    summary.permanent_failed = 0;

    for outcome in &summary.per_entry {
        match outcome.status {
            ExtractEntryStatus::Persisted => summary.persisted += 1,
            ExtractEntryStatus::FallbackPersisted => summary.fallback_persisted += 1,
            ExtractEntryStatus::DedupSkipped => summary.dedup_skipped += 1,
            ExtractEntryStatus::RetryableFailed => summary.retryable_failed += 1,
            ExtractEntryStatus::PermanentFailed => summary.permanent_failed += 1,
        }
    }
}
