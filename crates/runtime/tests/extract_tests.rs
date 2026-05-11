mod common;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::extract::{ArticleFetchTask, ExtractedArticle};
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_domain::state::{ContentQuality, ExtractorStrategy};
use rss_ai_news_extractor::{ContentStrategy, ExtractorError, HtmlFetcher, RawHtmlFetch};
use rss_ai_news_feed::fetcher::RawFeedFetch;
use rss_ai_news_feed::{FeedError, FeedFetcher};
use rss_ai_news_publish::LocalFsTarget;
use rss_ai_news_runtime::{
    ExtractEntryStatus, ExtractFlow, ExtractOptions, RunContext, RunContextDeps,
};
use rss_ai_news_storage::{
    ArticleRepository, NewArticle, SqliteArticleAiResultRepo, SqliteArticleRepo,
    SqliteFeedEntryRepo, SqliteFeedSourceRepo, SqlitePublishItemRepo, SqlitePublishRecordRepo,
    SqliteRawArtifactRepo, SqliteRunEventRepo,
};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use common::{
    DummyAiClient, app_config, insert_config_rule, insert_source, make_test_pool,
    seed_extractor_rule_version, seed_pending_fetch_entry,
};

struct MockHtmlFetcher {
    responses: Mutex<HashMap<i64, Result<RawHtmlFetch, ExtractorError>>>,
}

#[async_trait]
impl HtmlFetcher for MockHtmlFetcher {
    async fn fetch_html(&self, task: &ArticleFetchTask) -> Result<RawHtmlFetch, ExtractorError> {
        let response = {
            let mut guard = self.responses.lock().await;
            guard.remove(&task.feed_entry_id)
        };
        response.unwrap_or(Err(ExtractorError::ConnectionFailed))
    }
}

struct MockStrategy {
    strategy: ExtractorStrategy,
    extract_fn: Box<StrategyFn>,
}

type StrategyFn = dyn Fn(&ArticleFetchTask, &[u8], &str) -> Result<ExtractedArticle, ExtractorError>
    + Send
    + Sync;

impl ContentStrategy for MockStrategy {
    fn strategy(&self) -> ExtractorStrategy {
        self.strategy
    }

    fn extract(
        &self,
        task: &ArticleFetchTask,
        html_bytes: &[u8],
        final_url: &str,
    ) -> Result<ExtractedArticle, ExtractorError> {
        (self.extract_fn)(task, html_bytes, final_url)
    }
}

struct DummyFeedFetcher;

#[async_trait]
impl FeedFetcher for DummyFeedFetcher {
    async fn fetch_raw(&self, _req: &FeedFetchRequest) -> Result<RawFeedFetch, FeedError> {
        Err(FeedError::ConnectionFailed {
            source: "extract tests do not fetch feeds".to_string(),
        })
    }
}

#[tokio::test]
async fn extract_persists_new_article_on_success() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id =
        seed_pending_fetch_entry(&pool, source_id, "uid-success", "hash-success", None).await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Ok(raw(entry_id, b"<html>ok</html>")))]),
        vec![success_strategy("content-hash-success")],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let row: (String, Option<i64>) =
        sqlx::query_as("SELECT state, article_id FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("entry should be readable");
    let article_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM articles")
        .fetch_one(&pool)
        .await
        .expect("article count should be readable");
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts WHERE kind = 'html_payload'")
            .fetch_one(&pool)
            .await
            .expect("artifact count should be readable");

    assert_eq!(summary.persisted, 1);
    assert_eq!(summary.per_entry[0].status, ExtractEntryStatus::Persisted);
    assert_eq!(row.0, "persisted");
    assert!(row.1.is_some());
    assert_eq!(article_count, 1);
    assert_eq!(artifact_count, 1);
}

#[tokio::test]
async fn extract_dedup_skipped_when_content_hash_matches_existing_article() {
    let (_dir, pool) = make_test_pool().await;
    let (rule_id, source_id) = setup_base(&pool).await;
    let existing_entry_id =
        seed_pending_fetch_entry(&pool, source_id, "uid-existing", "hash-existing", None).await;
    let existing_article_id = SqliteArticleRepo::new(pool.clone())
        .insert_or_get_by_content_hash(&new_article("content-hash-dup", existing_entry_id, rule_id))
        .await
        .expect("existing article should insert")
        .article_id;
    sqlx::query("UPDATE feed_entries SET state = 'persisted', article_id = ? WHERE id = ?")
        .bind(existing_article_id)
        .bind(existing_entry_id)
        .execute(&pool)
        .await
        .expect("existing entry should be marked persisted");
    let entry_id = seed_pending_fetch_entry(&pool, source_id, "uid-dup", "hash-dup", None).await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Ok(raw(entry_id, b"<html>dup</html>")))]),
        vec![success_strategy("content-hash-dup")],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let row: (String, Option<i64>, Option<String>) =
        sqlx::query_as("SELECT state, article_id, dedup_decision FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("entry should be readable");
    let article_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM articles")
        .fetch_one(&pool)
        .await
        .expect("article count should be readable");

    assert_eq!(summary.dedup_skipped, 1);
    assert_eq!(row.0, "dedup_skipped");
    assert_eq!(row.1, Some(existing_article_id));
    assert_eq!(row.2, Some("hash_dup".to_string()));
    assert_eq!(article_count, 1);
}

#[tokio::test]
async fn extract_falls_back_to_summary_when_strategy_chain_fails() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id = seed_pending_fetch_entry(
        &pool,
        source_id,
        "uid-fallback",
        "hash-fallback",
        Some("<p>summary fallback body</p>"),
    )
    .await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Ok(raw(entry_id, b"<html>short</html>")))]),
        vec![parse_failed_strategy()],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let row: (String, Option<i64>) =
        sqlx::query_as("SELECT state, article_id FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("entry should be readable");
    let quality: String = sqlx::query_scalar("SELECT content_quality FROM articles WHERE id = ?")
        .bind(row.1)
        .fetch_one(&pool)
        .await
        .expect("fallback article should be readable");

    assert_eq!(summary.fallback_persisted, 1);
    assert_eq!(row.0, "fallback_persisted");
    assert_eq!(quality, "fallback");
}

#[tokio::test]
async fn extract_marks_failed_when_strategy_and_fallback_both_fail() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id =
        seed_pending_fetch_entry(&pool, source_id, "uid-failed", "hash-failed", None).await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Ok(raw(entry_id, b"<html>bad</html>")))]),
        vec![parse_failed_strategy()],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let state: String = sqlx::query_scalar("SELECT state FROM feed_entries WHERE id = ?")
        .bind(entry_id)
        .fetch_one(&pool)
        .await
        .expect("entry should be readable");
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_events WHERE event_kind = 'entry_permanent_failed'",
    )
    .fetch_one(&pool)
    .await
    .expect("event count should be readable");

    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(state, "failed");
    assert_eq!(event_count, 1);
}

#[tokio::test]
async fn extract_releases_retryable_on_5xx() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id = seed_pending_fetch_entry(&pool, source_id, "uid-503", "hash-503", None).await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Err(ExtractorError::HttpStatus { code: 503 }))]),
        vec![success_strategy("unused")],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let row: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT state, attempt_count, last_error_kind FROM feed_entries WHERE id = ?",
    )
    .bind(entry_id)
    .fetch_one(&pool)
    .await
    .expect("entry should be readable");

    assert_eq!(summary.retryable_failed, 1);
    assert_eq!(row.0, "pending_fetch");
    assert_eq!(row.1, 1);
    assert_eq!(row.2, Some("http_5xx".to_string()));
}

#[tokio::test]
async fn extract_marks_failed_on_4xx() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id = seed_pending_fetch_entry(&pool, source_id, "uid-404", "hash-404", None).await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Err(ExtractorError::HttpStatus { code: 404 }))]),
        vec![success_strategy("unused")],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error_kind FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("entry should be readable");

    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(row.0, "failed");
    assert_eq!(row.1, Some("http_4xx".to_string()));
}

#[tokio::test]
async fn extract_writes_html_artifact_before_strategy() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id =
        seed_pending_fetch_entry(&pool, source_id, "uid-artifact", "hash-artifact", None).await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Ok(raw(entry_id, b"<html>artifact</html>")))]),
        vec![parse_failed_strategy()],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let artifact_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM raw_artifacts WHERE kind = 'html_payload' AND artifact_key = ?",
    )
    .bind(entry_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("artifact count should be readable");

    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(artifact_count, 1);
}

async fn setup_base(pool: &SqlitePool) -> (i64, i64) {
    let extractor_rule_id = seed_extractor_rule_version(pool).await;
    let config_id = insert_config_rule(pool).await;
    let source_id = insert_source(
        pool,
        config_id,
        "extract-source",
        "https://example.com/feed.xml",
    )
    .await;
    (extractor_rule_id, source_id)
}

fn flow(
    pool: SqlitePool,
    responses: HashMap<i64, Result<RawHtmlFetch, ExtractorError>>,
    strategies: Vec<Arc<dyn ContentStrategy>>,
) -> ExtractFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let ctx = Arc::new(RunContext::new_for_stage(
        "extract",
        app,
        RunContextDeps {
            feed_fetcher: Arc::new(DummyFeedFetcher),
            html_fetcher: Arc::new(MockHtmlFetcher {
                responses: Mutex::new(responses),
            }),
            strategies,
            ai_client: Arc::new(DummyAiClient),
            publish_target_local: Arc::new(LocalFsTarget::new(std::env::temp_dir())),
            publish_target_remote: None,
            feed_source_repo: Arc::new(SqliteFeedSourceRepo::new(pool.clone())),
            feed_entry_repo: Arc::new(SqliteFeedEntryRepo::new(pool.clone())),
            article_repo: Arc::new(SqliteArticleRepo::new(pool.clone())),
            ai_result_repo: Arc::new(SqliteArticleAiResultRepo::new(pool.clone())),
            publish_record_repo: Arc::new(SqlitePublishRecordRepo::new(pool.clone())),
            publish_item_repo: Arc::new(SqlitePublishItemRepo::new(pool.clone())),
            artifact_repo: Arc::new(SqliteRawArtifactRepo::new(pool.clone())),
            event_repo: Arc::new(SqliteRunEventRepo::new(pool.clone())),
            rule_version_repo: Arc::new(rss_ai_news_storage::SqliteRuleVersionRepo::new(pool)),
        },
    ));
    ExtractFlow::new(ctx)
}

fn responses(
    items: impl IntoIterator<Item = (i64, Result<RawHtmlFetch, ExtractorError>)>,
) -> HashMap<i64, Result<RawHtmlFetch, ExtractorError>> {
    items.into_iter().collect()
}

fn raw(feed_entry_id: i64, body: &[u8]) -> RawHtmlFetch {
    RawHtmlFetch {
        feed_entry_id,
        final_url: format!("https://example.com/final/{feed_entry_id}"),
        http_status: 200,
        body_bytes: body.to_vec(),
    }
}

fn success_strategy(content_hash: &str) -> Arc<dyn ContentStrategy> {
    let content_hash = content_hash.to_string();
    Arc::new(MockStrategy {
        strategy: ExtractorStrategy::Readability,
        extract_fn: Box::new(move |task, _bytes, final_url| {
            Ok(ExtractedArticle {
                feed_entry_id: task.feed_entry_id,
                canonical_link: final_url.to_string(),
                title: task.title_raw.clone(),
                body_text: "body text with enough words".to_string(),
                body_html: Some(b"<article>body</article>".to_vec()),
                extractor_strategy: ExtractorStrategy::Readability,
                content_quality: ContentQuality::High,
                word_count: 5,
                content_hash: content_hash.clone(),
            })
        }),
    })
}

fn parse_failed_strategy() -> Arc<dyn ContentStrategy> {
    Arc::new(MockStrategy {
        strategy: ExtractorStrategy::Readability,
        extract_fn: Box::new(|_task, _bytes, _final_url| {
            Err(ExtractorError::ParseFailed {
                reason: "mock parse failure".to_string(),
            })
        }),
    })
}

// === F6-3 N3: max_batches enforcement (W2 DeepSeek 复审) ===

#[tokio::test]
async fn max_batches_caps_loop_and_reports_reached_flag() {
    // 3 个 pending entry，batch_size=1, max_batches=2 ⇒ 应处理 2 行、
    // 第 3 行保留为 pending，summary.max_batches_reached=true。
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let e1 = seed_pending_fetch_entry(&pool, source_id, "uid-a", "hash-a", None).await;
    let e2 = seed_pending_fetch_entry(&pool, source_id, "uid-b", "hash-b", None).await;
    let _e3 = seed_pending_fetch_entry(&pool, source_id, "uid-c", "hash-c", None).await;

    let flow = flow(
        pool.clone(),
        responses([
            (e1, Ok(raw(e1, b"<html>a</html>"))),
            (e2, Ok(raw(e2, b"<html>b</html>"))),
            // e3 不放 response，避免被处理时 mock 报错；反正不应被 claim
        ]),
        vec![parse_failed_strategy()],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 1,
            max_batches: 2,
        })
        .await;

    assert_eq!(summary.batches_executed, 2);
    assert_eq!(summary.claimed, 2);
    assert!(summary.max_batches_reached, "should hit cap with 3 pending");

    let pending_left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feed_entries WHERE state = 'pending_fetch'")
            .fetch_one(&pool)
            .await
            .expect("count pending");
    assert_eq!(pending_left, 1, "third entry must remain pending");
}

#[tokio::test]
async fn max_batches_zero_means_unlimited_until_queue_drained() {
    // 3 个 pending entry，batch_size=1, max_batches=0 ⇒ 全部处理完、
    // max_batches_reached=false（自然耗尽）。
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let e1 = seed_pending_fetch_entry(&pool, source_id, "uid-a", "hash-a", None).await;
    let e2 = seed_pending_fetch_entry(&pool, source_id, "uid-b", "hash-b", None).await;
    let e3 = seed_pending_fetch_entry(&pool, source_id, "uid-c", "hash-c", None).await;

    let flow = flow(
        pool.clone(),
        responses([
            (e1, Ok(raw(e1, b"<html>a</html>"))),
            (e2, Ok(raw(e2, b"<html>b</html>"))),
            (e3, Ok(raw(e3, b"<html>c</html>"))),
        ]),
        vec![parse_failed_strategy()],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 1,
            max_batches: 0,
        })
        .await;

    assert_eq!(summary.claimed, 3);
    assert_eq!(summary.batches_executed, 3);
    assert!(!summary.max_batches_reached);

    let pending_left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feed_entries WHERE state = 'pending_fetch'")
            .fetch_one(&pool)
            .await
            .expect("count pending");
    assert_eq!(pending_left, 0);
}

fn new_article(content_hash: &str, entry_id: i64, rule_id: i64) -> NewArticle {
    NewArticle {
        content_hash: content_hash.to_string(),
        canonical_link: "https://example.com/existing".to_string(),
        title: "existing".to_string(),
        body_text: "existing body".to_string(),
        body_html_artifact_id: None,
        extractor_strategy: "readability".to_string(),
        extractor_version: rule_id,
        content_quality: "high".to_string(),
        word_count: 2,
        origin_feed_entry_id: entry_id,
    }
}
