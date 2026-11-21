//! Extract flow under storage failures (A3): a retryable `StorageError`
//! (pool timeout, PG 40001/40P01, SQLite busy) goes through the budgeted
//! cooldown release with its real `error_kind`; only non-retryable storage
//! errors terminalize the entry (plan/11 §3, plan/15 §3).

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::extract::{ArticleFetchTask, ExtractedArticle};
use rss_ai_news_domain::model::Article;
use rss_ai_news_domain::state::{ContentQuality, ExtractorStrategy};
use rss_ai_news_extractor::{ContentStrategy, ExtractorError, HtmlFetcher, RawHtmlFetch};
use rss_ai_news_runtime::{ExtractFlow, ExtractOptions};
use rss_ai_news_storage::{
    ArticleAiTaskCandidate, ArticleContentHash, ArticleInsertOutcome, ArticleRepo,
    ArticleRepository, BackfillArticleCandidate, ClaimRequest, ClaimedFeedEntry,
    ContentHashReindexCandidate, EntryPersistOutcome, FeedEntry, FeedEntryInsertOutcome,
    FeedEntryRepo, FeedEntryRepository, LinkHashReindexCandidate, NewArticle, NewFeedEntry,
    RecentFeedEntry, RecentFeedEntryFilter, ReleaseFailureOutcome, ResetFailedFilter,
    ResetFailedOutcome, StorageError, UpdateContentHashOutcome, UpdateLinkHashOutcome,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use common::{
    app_config, insert_config_rule, insert_source, make_test_pool, seed_extractor_rule_version,
    seed_pending_fetch_entry,
};

/// Fails the next `remaining` calls of one method with `error()`.
struct FailNext {
    remaining: AtomicU32,
    error: fn() -> StorageError,
}

impl FailNext {
    fn new(times: u32, error: fn() -> StorageError) -> Self {
        Self {
            remaining: AtomicU32::new(times),
            error,
        }
    }

    fn none() -> Self {
        Self::new(0, retryable)
    }

    fn take(&self) -> Option<StorageError> {
        self.remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .ok()
            .map(|_| (self.error)())
    }
}

fn retryable() -> StorageError {
    StorageError::Unavailable("injected transient failure".to_string())
}

fn non_retryable() -> StorageError {
    StorageError::Corruption("injected permanent failure".to_string())
}

struct FlakyArticleRepo {
    inner: ArticleRepo,
    insert: FailNext,
}

#[async_trait]
impl ArticleRepository for FlakyArticleRepo {
    async fn insert_or_get_by_content_hash(
        &self,
        article: &NewArticle,
    ) -> Result<ArticleInsertOutcome, StorageError> {
        if let Some(error) = self.insert.take() {
            return Err(error);
        }
        self.inner.insert_or_get_by_content_hash(article).await
    }
    async fn persist_for_entry(
        &self,
        article: &NewArticle,
        owner: &str,
        fallback: bool,
        now: OffsetDateTime,
    ) -> Result<EntryPersistOutcome, StorageError> {
        if let Some(error) = self.insert.take() {
            return Err(error);
        }
        self.inner
            .persist_for_entry(article, owner, fallback, now)
            .await
    }
    async fn find_by_id(&self, id: i64) -> Result<Option<Article>, StorageError> {
        self.inner.find_by_id(id).await
    }
    async fn list_backlog_for_ai_task_gen(
        &self,
        category_key: &str,
        identity: &rss_ai_news_storage::NewAiResult,
        published_since: OffsetDateTime,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
        self.inner
            .list_backlog_for_ai_task_gen(
                category_key,
                identity,
                published_since,
                batch_size,
                after_id,
            )
            .await
    }
    async fn list_fresh_for_ai_task_gen(
        &self,
        category_key: &str,
        identity: &rss_ai_news_storage::NewAiResult,
        published_since: OffsetDateTime,
        batch_size: u32,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
        self.inner
            .list_fresh_for_ai_task_gen(category_key, identity, published_since, batch_size)
            .await
    }
    async fn list_in_window_for_backfill(
        &self,
        date_from: Option<OffsetDateTime>,
        date_to: Option<OffsetDateTime>,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<BackfillArticleCandidate>, StorageError> {
        self.inner
            .list_in_window_for_backfill(date_from, date_to, batch_size, after_id)
            .await
    }
    async fn list_for_content_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<ContentHashReindexCandidate>, StorageError> {
        self.inner
            .list_for_content_hash_reindex(after_id, batch_size)
            .await
    }
    async fn list_content_hashes(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<ArticleContentHash>, StorageError> {
        self.inner.list_content_hashes(after_id, batch_size).await
    }
    async fn update_content_hash(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError> {
        self.inner.update_content_hash(id, new_content_hash).await
    }
    async fn peek_content_hash_outcome(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError> {
        self.inner
            .peek_content_hash_outcome(id, new_content_hash)
            .await
    }
}

struct FlakyFeedEntryRepo {
    inner: FeedEntryRepo,
    find_by_id: FailNext,
    release_permanent: FailNext,
}

#[async_trait]
impl FeedEntryRepository for FlakyFeedEntryRepo {
    async fn insert_deduplicated(
        &self,
        entry: &NewFeedEntry,
    ) -> Result<FeedEntryInsertOutcome, StorageError> {
        self.inner.insert_deduplicated(entry).await
    }
    async fn exists_by_link_hash(&self, link_hash: &str) -> Result<bool, StorageError> {
        self.inner.exists_by_link_hash(link_hash).await
    }
    async fn find_by_id(&self, id: i64) -> Result<Option<FeedEntry>, StorageError> {
        if let Some(error) = self.find_by_id.take() {
            return Err(error);
        }
        self.inner.find_by_id(id).await
    }
    async fn list_recent(
        &self,
        filter: &RecentFeedEntryFilter,
    ) -> Result<Vec<RecentFeedEntry>, StorageError> {
        FeedEntryRepository::list_recent(&self.inner, filter).await
    }
    async fn claim_pending_fetch_in_scope(
        &self,
        request: &ClaimRequest,
        exclude_ids: &[i64],
        category_key: Option<&str>,
    ) -> Result<Vec<ClaimedFeedEntry>, StorageError> {
        self.inner
            .claim_pending_fetch_in_scope(request, exclude_ids, category_key)
            .await
    }
    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        self.inner.release_success(id, owner, article_id, now).await
    }
    async fn release_retryable_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        retry_at: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError> {
        self.inner
            .release_retryable_failure(id, owner, error, kind, max_attempts, retry_at, now)
            .await
    }
    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        if let Some(error) = self.release_permanent.take() {
            return Err(error);
        }
        self.inner
            .release_permanent_failure(id, owner, error, kind, now)
            .await
    }
    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        self.inner.reclaim_expired_leases(now).await
    }
    async fn terminalize_exhausted(
        &self,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        self.inner.terminalize_exhausted(max_attempts, now).await
    }
    async fn release_dedup_skipped(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        decision: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        self.inner
            .release_dedup_skipped(id, owner, article_id, decision, now)
            .await
    }
    async fn release_fallback_persisted(
        &self,
        id: i64,
        owner: &str,
        article_id: i64,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        self.inner
            .release_fallback_persisted(id, owner, article_id, now)
            .await
    }
    async fn reset_failed_in_window(
        &self,
        filter: &ResetFailedFilter,
    ) -> Result<ResetFailedOutcome, StorageError> {
        self.inner.reset_failed_in_window(filter).await
    }
    async fn list_for_link_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<LinkHashReindexCandidate>, StorageError> {
        self.inner
            .list_for_link_hash_reindex(after_id, batch_size)
            .await
    }
    async fn update_link_hash(
        &self,
        id: i64,
        new_link_hash: &str,
    ) -> Result<UpdateLinkHashOutcome, StorageError> {
        self.inner.update_link_hash(id, new_link_hash).await
    }
}

struct MapFetcher {
    responses: Mutex<HashMap<i64, Result<RawHtmlFetch, ExtractorError>>>,
}

#[async_trait]
impl HtmlFetcher for MapFetcher {
    async fn fetch_html(&self, task: &ArticleFetchTask) -> Result<RawHtmlFetch, ExtractorError> {
        let response = self.responses.lock().await.remove(&task.feed_entry_id);
        response.unwrap_or(Err(ExtractorError::ConnectionFailed))
    }
}

struct FixedStrategy;

impl ContentStrategy for FixedStrategy {
    fn strategy(&self) -> ExtractorStrategy {
        ExtractorStrategy::Readability
    }

    fn extract(
        &self,
        task: &ArticleFetchTask,
        _html_bytes: &[u8],
        final_url: &str,
    ) -> Result<ExtractedArticle, ExtractorError> {
        Ok(ExtractedArticle {
            feed_entry_id: task.feed_entry_id,
            canonical_link: final_url.to_string(),
            title: task.title_raw.clone(),
            body_text: "body text with enough words".to_string(),
            body_html: None,
            extractor_strategy: ExtractorStrategy::Readability,
            content_quality: ContentQuality::High,
            word_count: 5,
            content_hash: format!("content-hash-{}", task.feed_entry_id),
        })
    }
}

struct Setup {
    _dir: std::path::PathBuf,
    pool: SqlitePool,
    entry_id: i64,
}

async fn setup(summary_raw: Option<&str>) -> Setup {
    let (dir, pool) = make_test_pool().await;
    seed_extractor_rule_version(&pool).await;
    let config_id = insert_config_rule(&pool).await;
    let source_id = insert_source(&pool, config_id, "src", "https://example.com/feed.xml").await;
    let entry_id = seed_pending_fetch_entry(&pool, source_id, "uid-1", "hash-1", summary_raw).await;
    Setup {
        _dir: dir,
        pool,
        entry_id,
    }
}

fn ok_html(entry_id: i64) -> Result<RawHtmlFetch, ExtractorError> {
    Ok(RawHtmlFetch {
        feed_entry_id: entry_id,
        final_url: format!("https://example.com/final/{entry_id}"),
        http_status: 200,
        content_type: None,
        body_bytes: b"<html>ok</html>".to_vec(),
    })
}

fn flow(
    setup: &Setup,
    response: Result<RawHtmlFetch, ExtractorError>,
    strategies: Vec<Arc<dyn ContentStrategy>>,
    article_insert: FailNext,
    find_by_id: FailNext,
    release_permanent: FailNext,
) -> ExtractFlow {
    let app = Arc::new(app_config(RetentionPolicy::Off, 1));
    let fetcher = Arc::new(MapFetcher {
        responses: Mutex::new(HashMap::from([(setup.entry_id, response)])),
    });
    let mut deps = common::extract_deps(setup.pool.clone(), app, fetcher, strategies);
    deps.article_repo = Arc::new(FlakyArticleRepo {
        inner: ArticleRepo::new(setup.pool.clone()),
        insert: article_insert,
    });
    deps.feed_entry_repo = Arc::new(FlakyFeedEntryRepo {
        inner: FeedEntryRepo::new(setup.pool.clone()),
        find_by_id,
        release_permanent,
    });
    ExtractFlow::new(Arc::new(deps))
}

fn opts(max_attempts: u32) -> ExtractOptions {
    ExtractOptions {
        batch_size: 1,
        max_attempts,
        max_batches: 0,
        category_key: None,
    }
}

async fn entry_row(
    pool: &SqlitePool,
    id: i64,
) -> (String, i64, Option<String>, Option<OffsetDateTime>) {
    sqlx::query_as(
        "SELECT state, attempt_count, last_error_kind, next_attempt_at FROM feed_entries WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("entry row")
}

#[tokio::test]
async fn transient_article_insert_failure_is_retried_with_cooldown() {
    let setup = setup(None).await;
    let flow = flow(
        &setup,
        ok_html(setup.entry_id),
        vec![Arc::new(FixedStrategy)],
        FailNext::new(1, retryable),
        FailNext::none(),
        FailNext::none(),
    );

    let summary = flow.run(opts(5)).await;
    let (state, attempts, kind, next_attempt_at) = entry_row(&setup.pool, setup.entry_id).await;

    assert_eq!(summary.retryable_failed, 1, "{summary:?}");
    assert_eq!(summary.permanent_failed, 0);
    assert_eq!(summary.storage_failed, 1, "counted as a storage failure");
    assert_eq!(state, "pending_fetch");
    assert_eq!(attempts, 1, "budget not exhausted");
    assert_eq!(kind.as_deref(), Some("db_unavailable"), "real error kind");
    assert!(
        next_attempt_at.is_some_and(|at| at > OffsetDateTime::now_utc()),
        "cooldown set"
    );
}

#[tokio::test]
async fn transient_article_insert_failure_on_last_attempt_folds_to_failed() {
    let setup = setup(None).await;
    // Effective budget is max(opts, config) = 5; attempt 5 is the last one.
    sqlx::query("UPDATE feed_entries SET attempt_count = 4 WHERE id = ?")
        .bind(setup.entry_id)
        .execute(&setup.pool)
        .await
        .expect("prime attempt_count");
    let flow = flow(
        &setup,
        ok_html(setup.entry_id),
        vec![Arc::new(FixedStrategy)],
        FailNext::new(1, retryable),
        FailNext::none(),
        FailNext::none(),
    );

    let summary = flow.run(opts(5)).await;
    let (state, _, kind, _) = entry_row(&setup.pool, setup.entry_id).await;

    assert_eq!(summary.permanent_failed, 1, "{summary:?}");
    assert_eq!(state, "failed");
    assert_eq!(kind.as_deref(), Some("db_unavailable"));
}

#[tokio::test]
async fn non_retryable_article_insert_failure_is_permanent() {
    let setup = setup(None).await;
    let flow = flow(
        &setup,
        ok_html(setup.entry_id),
        vec![Arc::new(FixedStrategy)],
        FailNext::new(1, non_retryable),
        FailNext::none(),
        FailNext::none(),
    );

    let summary = flow.run(opts(5)).await;
    let (state, _, kind, _) = entry_row(&setup.pool, setup.entry_id).await;

    assert_eq!(summary.permanent_failed, 1, "{summary:?}");
    assert_eq!(state, "failed");
    assert_eq!(kind.as_deref(), Some("corruption"));
}

#[tokio::test]
async fn transient_summary_read_failure_after_403_is_not_terminal() {
    let setup = setup(Some("<p>feed summary text</p>")).await;
    let flow = flow(
        &setup,
        Err(ExtractorError::HttpStatus { code: 403 }),
        vec![Arc::new(FixedStrategy)],
        FailNext::none(),
        FailNext::new(1, retryable),
        FailNext::none(),
    );

    let summary = flow.run(opts(5)).await;
    let (state, attempts, kind, next_attempt_at) = entry_row(&setup.pool, setup.entry_id).await;

    assert_eq!(summary.retryable_failed, 1, "{summary:?}");
    assert_eq!(state, "pending_fetch");
    assert_eq!(attempts, 1);
    assert_eq!(kind.as_deref(), Some("db_unavailable"));
    assert!(next_attempt_at.is_some());
}

#[tokio::test]
async fn transient_summary_read_failure_after_parse_failure_is_not_terminal() {
    let setup = setup(Some("<p>feed summary text</p>")).await;
    let flow = flow(
        &setup,
        ok_html(setup.entry_id),
        Vec::new(),
        FailNext::none(),
        FailNext::new(1, retryable),
        FailNext::none(),
    );

    let summary = flow.run(opts(5)).await;
    let (state, _, kind, _) = entry_row(&setup.pool, setup.entry_id).await;

    assert_eq!(summary.retryable_failed, 1, "{summary:?}");
    assert_eq!(state, "pending_fetch");
    assert_eq!(kind.as_deref(), Some("db_unavailable"));
}

/// A5: a release that itself fails is `ReleaseFailed` (storage failure), never
/// reported as `PermanentFailed`, and writes no `entry_permanent_failed` event.
#[tokio::test]
async fn failed_permanent_release_reports_release_failed() {
    let setup = setup(None).await;
    let flow = flow(
        &setup,
        Err(ExtractorError::HttpStatus { code: 404 }),
        vec![Arc::new(FixedStrategy)],
        FailNext::none(),
        FailNext::none(),
        FailNext::new(1, retryable),
    );

    let summary = flow.run(opts(5)).await;
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_events WHERE event_kind = 'entry_permanent_failed'",
    )
    .fetch_one(&setup.pool)
    .await
    .expect("event count");
    let (state, _, _, _) = entry_row(&setup.pool, setup.entry_id).await;

    assert_eq!(summary.permanent_failed, 0, "{summary:?}");
    assert_eq!(summary.release_failed, 1);
    assert_eq!(summary.storage_failed, 1);
    assert_eq!(events, 0);
    assert_eq!(state, "fetching", "lease stays until expiry reclaim");
}
