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
}

#[derive(Debug, Default, Clone)]
pub struct ExtractSummary {
    pub claimed: u32,
    pub persisted: u32,
    pub fallback_persisted: u32,
    pub dedup_skipped: u32,
    pub retryable_failed: u32,
    pub permanent_failed: u32,
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
    /// `true` 表示本 run 至少有一行 retryable 失败被放回 `pending_fetch` 并进入
    /// cooldown，重试推迟到之后的 run。它不再终止循环（retryable 失败后继续
    /// 领取其余可领取行），因此可与 `max_batches_reached` 同时为 `true`。
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
        // Retry cooldown cutoff for every claim in this run. Each retryable
        // release writes `next_attempt_at > run_started_at`, so an entry that
        // failed in this run is never reclaimed by it, however long it runs.
        let run_started_at = OffsetDateTime::now_utc();
        // W15 §3.3：claim 过滤与 release 折叠必须用同一预算值，hoist 一处计算。
        let max_attempts = opts
            .max_attempts
            .max(self.ctx.retry.feed_entry_max_attempts);

        // W15 §5：首次 claim 前执行一次 ① reclaim + ② sweep（顺序固定，best-effort）。
        let maintenance_now = OffsetDateTime::now_utc();
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

            let now = OffsetDateTime::now_utc();
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
                .claim_pending_fetch(&request, run_started_at)
                .await
            {
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
                    summary.claim_error = Some(error.error_kind().to_string());
                    return summary;
                }
            };

            // 自然耗尽：本批次 claim 到空集，没有未冷却的 pending 行。
            if claimed.is_empty() {
                break;
            }

            summary.claimed += claimed.len() as u32;
            summary.batches_executed += 1;

            let concurrency = self.ctx.http.concurrent_fetches.max(1) as usize;
            let mut join_set = JoinSet::new();
            let mut pending = claimed.into_iter();
            loop {
                // Only materialize task state for the next concurrency window.
                while join_set.len() < concurrency {
                    let Some(entry) = pending.next() else { break };
                    let ctx = Arc::clone(&self.ctx);
                    let owner = owner.clone();
                    join_set.spawn(Self::process_entry(
                        ctx,
                        owner,
                        entry,
                        max_attempts,
                        run_started_at,
                    ));
                }
                let Some(result) = join_set.join_next().await else {
                    break;
                };
                match result {
                    Ok(outcome) => summary.record(outcome),
                    Err(error) => {
                        tracing::error!("extract entry task panicked or was cancelled: {error}");
                        summary.tasks_panicked += 1;
                    }
                }
            }
            // Retryable failures do not stop the loop: their rows went back to
            // pending_fetch with a cooldown past `run_started_at`, so the next
            // claim skips them and only takes other claimable work.
        }
        if summary.retryable_deferred {
            tracing::info!(
                stage = "extract",
                retryable_failed = summary.retryable_failed,
                batches_executed = summary.batches_executed,
                "retryable failures are cooling down; retries deferred to later runs"
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
                    "tasks_panicked": summary.tasks_panicked,
                    "batches_executed": summary.batches_executed,
                    "max_batches_reached": summary.max_batches_reached,
                    "retryable_deferred": summary.retryable_deferred,
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
        run_started_at: OffsetDateTime,
    ) -> ExtractEntryOutcome {
        let emitter = RunEventEmitter {
            run_id: &ctx.run.run_id,
            stage: "extract",
            repo: ctx.event_repo.as_ref(),
        };
        let retry = RetrySchedule {
            max_attempts,
            attempt_count: claimed.attempt_count,
            run_started_at,
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
                let now = OffsetDateTime::now_utc();
                // W18（plan/17）：永久性抓取失败（403 付费墙 / 404 / too_large）
                // 且 feed 摘要可用时降级成文，与下方解析链失败分支对称。
                // 可重试错误（超时/5xx）不消费摘要——重试仍有机会拿到全文，
                // W15 预算路径维持不变。
                if !error.is_retryable()
                    && let Some(fallback) = load_summary_fallback(&ctx, &fetch_task).await
                {
                    tracing::info!(
                        feed_entry_id = claimed.id,
                        error_kind = error.error_kind(),
                        "permanent fetch failure; falling back to feed summary"
                    );
                    return persist_fallback(
                        &ctx, &emitter, &owner, claimed.id, fallback, None, now,
                    )
                    .await;
                }
                return release_extract_error(
                    &ctx, &emitter, claimed.id, &owner, error, retry, now,
                )
                .await;
            }
        };

        let now = OffsetDateTime::now_utc();
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
                release_extract_error(&ctx, &emitter, claimed.id, &owner, error, retry, now).await
            }
            ChainResult::Failed(errors) => {
                if let Some(fallback) = load_summary_fallback(&ctx, &fetch_task).await {
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

async fn load_summary_fallback(
    ctx: &ExtractDeps,
    task: &ArticleFetchTask,
) -> Option<FallbackArticle> {
    match ctx.feed_entry_repo.find_by_id(task.feed_entry_id).await {
        Ok(Some(entry)) => {
            let mut fallback_task = task.clone();
            fallback_task.summary_raw = entry.summary_raw;
            summary_fallback(&fallback_task)
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                feed_entry_id = task.feed_entry_id,
                "failed to load fallback summary: {error}"
            );
            None
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
    let mut errors = Vec::new();
    for strategy in &ctx.strategies {
        match strategy.extract(task, &raw.body_bytes, &raw.final_url) {
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

async fn persist_extracted(
    ctx: &ExtractDeps,
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
    ctx: &ExtractDeps,
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
    ctx: &ExtractDeps,
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
    ctx: &ExtractDeps,
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
    ctx: &ExtractDeps,
    emitter: &RunEventEmitter<'_>,
    feed_entry_id: i64,
    owner: &str,
    error: ExtractorError,
    retry: RetrySchedule,
    now: OffsetDateTime,
) -> ExtractEntryOutcome {
    let message = error.display_user();
    let kind = error.error_kind().to_string();
    if error.is_retryable() {
        // W15 §3：release SQL 内按预算折叠——耗尽即转 failed 终态。
        let outcome = match ctx
            .feed_entry_repo
            .release_retryable_failure(
                feed_entry_id,
                owner,
                &message,
                &kind,
                retry.max_attempts,
                retry.next_attempt_at(now),
                now,
            )
            .await
        {
            Ok(outcome) => {
                if !outcome.released {
                    tracing::warn!(feed_entry_id, "retryable release conflicted");
                }
                outcome
            }
            Err(error) => {
                tracing::error!(feed_entry_id, "retryable release failed: {error}");
                rss_ai_news_storage::ReleaseFailureOutcome {
                    released: false,
                    exhausted: false,
                }
            }
        };
        if outcome.exhausted {
            emitter
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
            return ExtractEntryOutcome {
                feed_entry_id,
                status: ExtractEntryStatus::PermanentFailed,
                article_id: None,
                error_kind: Some(kind),
            };
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
    ctx: &ExtractDeps,
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
    run_started_at: OffsetDateTime,
}

impl RetrySchedule {
    /// When a retryable failure released at `now` becomes claimable again.
    ///
    /// Post: the result is strictly after `run_started_at` (the claim cutoff),
    /// even if the wall clock stepped back during the run, so the releasing run
    /// never reclaims the entry.
    fn next_attempt_at(&self, now: OffsetDateTime) -> OffsetDateTime {
        now.max(self.run_started_at) + retry_backoff(self.attempt_count)
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
        }
        if matches!(
            outcome.status,
            ExtractEntryStatus::RetryableFailed | ExtractEntryStatus::PermanentFailed
        ) && self.failure_samples.len() < 32
        {
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
    fn next_attempt_is_strictly_after_run_start() {
        let run_started_at = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let retry = RetrySchedule {
            max_attempts: 5,
            attempt_count: 1,
            run_started_at,
        };
        // Normal case: failure after run start.
        let later = run_started_at + Duration::seconds(30);
        assert_eq!(retry.next_attempt_at(later), later + Duration::minutes(10));
        // Wall clock stepped back below run start: still after the cutoff.
        let earlier = run_started_at - Duration::hours(1);
        assert_eq!(
            retry.next_attempt_at(earlier),
            run_started_at + Duration::minutes(10)
        );
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
            });
            summary.record(ExtractEntryOutcome {
                feed_entry_id: 1,
                article_id: None,
                status: ExtractEntryStatus::PermanentFailed,
                error_kind: Some("failure".into()),
            });
        }
        summary.record(ExtractEntryOutcome {
            feed_entry_id: 1,
            article_id: None,
            status: ExtractEntryStatus::RetryableFailed,
            error_kind: None,
        });
        assert_eq!(summary.persisted, 10_000);
        assert_eq!(summary.permanent_failed, 10_000);
        assert_eq!(summary.retryable_failed, 1);
        assert_eq!(summary.failure_samples.len(), 32);
        assert_eq!(summary.tasks_panicked, 4);
    }
}
