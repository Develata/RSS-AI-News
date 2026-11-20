mod common;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::extract::{ArticleFetchTask, ExtractedArticle};
use rss_ai_news_domain::state::{ContentQuality, ExtractorStrategy};
use rss_ai_news_extractor::{ContentStrategy, ExtractorError, HtmlFetcher, RawHtmlFetch};
use rss_ai_news_runtime::{ExtractFlow, ExtractOptions};
use rss_ai_news_storage::{ArticleRepo, ArticleRepository, NewArticle};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use common::{
    app_config, insert_config_rule, insert_source, make_test_pool, seed_extractor_rule_version,
    seed_pending_fetch_entry,
};

struct MockHtmlFetcher {
    responses: Mutex<HashMap<i64, Result<RawHtmlFetch, ExtractorError>>>,
}

#[async_trait]
impl HtmlFetcher for MockHtmlFetcher {
    async fn fetch_html(&self, task: &ArticleFetchTask) -> Result<RawHtmlFetch, ExtractorError> {
        assert!(
            task.summary_raw.is_none(),
            "fetch path must not eagerly load fallback summary"
        );
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
    assert!(summary.failure_samples.is_empty());
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
    let existing_article_id = ArticleRepo::new(pool.clone())
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

/// W18（plan/17）：403 等永久性抓取失败 + feed 摘要可用 → 摘要成文，
/// 不再整条作废。生产实证：付费墙源（FT/NYT/OpenAI）3312 条 http_4xx。
#[tokio::test]
async fn extract_fetch_403_with_summary_persists_fallback() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id = seed_pending_fetch_entry(
        &pool,
        source_id,
        "uid-403-fb",
        "hash-403-fb",
        Some("<p>paywalled article summary from feed</p>"),
    )
    .await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Err(ExtractorError::HttpStatus { code: 403 }))]),
        vec![success_strategy("unused")],
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
    let (quality, strategy, artifact_id): (String, String, Option<i64>) = sqlx::query_as(
        "SELECT content_quality, extractor_strategy, body_html_artifact_id \
         FROM articles WHERE id = ?",
    )
    .bind(row.1)
    .fetch_one(&pool)
    .await
    .expect("fallback article should be readable");

    assert_eq!(summary.fallback_persisted, 1);
    assert_eq!(summary.permanent_failed, 0);
    assert_eq!(row.0, "fallback_persisted");
    assert_eq!(quality, "fallback");
    assert_eq!(strategy, "summary_fallback");
    assert_eq!(artifact_id, None, "no html fetched, no artifact to persist");
}

/// W18：403 且 feed 无摘要 → 维持旧行为转 failed(http_4xx)，锁住不回归。
#[tokio::test]
async fn extract_fetch_403_without_summary_fails_permanent() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id =
        seed_pending_fetch_entry(&pool, source_id, "uid-403-dead", "hash-403-dead", None).await;
    let flow = flow(
        pool.clone(),
        responses([(entry_id, Err(ExtractorError::HttpStatus { code: 403 }))]),
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

/// W18：可重试错误（5xx）即使摘要可用也不提前降级——重试仍有机会拿全文，
/// W15 预算路径不变。
#[tokio::test]
async fn extract_fetch_5xx_with_summary_stays_retryable() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id = seed_pending_fetch_entry(
        &pool,
        source_id,
        "uid-503-summary",
        "hash-503-summary",
        Some("<p>summary exists but must not be consumed yet</p>"),
    )
    .await;
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
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error_kind FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("entry should be readable");

    assert_eq!(summary.retryable_failed, 1);
    assert_eq!(summary.fallback_persisted, 0);
    assert_eq!(row.0, "pending_fetch");
    assert_eq!(row.1, Some("http_5xx".to_string()));
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
    // retryable_deferred = 本 run 有行进入 cooldown、重试推迟到后续 run；
    // 循环本身因 claim 为空（无其他可领取行）自然结束，未命中上限。
    assert!(summary.retryable_deferred);
    assert!(!summary.max_batches_reached);
}

#[tokio::test]
async fn extract_retryable_on_final_attempt_folds_to_failed() {
    // W15 §3：有效预算 = max(opts.max_attempts, config.feed_entry_max_attempts=5)。
    // 预置 attempt_count=4，本次 claim 自增到 5 即预算内最后一次尝试；
    // retryable 503 由 release SQL 内折叠 → failed（改前回 pending_fetch 卡死）。
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id =
        seed_pending_fetch_entry(&pool, source_id, "uid-exhaust", "hash-exhaust", None).await;
    sqlx::query("UPDATE feed_entries SET attempt_count = 4 WHERE id = ?")
        .bind(entry_id)
        .execute(&pool)
        .await
        .expect("prime attempt_count");
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

    // 计入永久失败而非"将重试"；折叠保留真实错误。
    assert_eq!(summary.permanent_failed, 1);
    assert_eq!(summary.retryable_failed, 0);
    assert!(!summary.retryable_deferred);
    assert_eq!(row.0, "failed");
    assert_eq!(row.1, 5);
    assert_eq!(row.2, Some("http_5xx".to_string()));
    // entry_permanent_failed 事件带 budget_exhausted（15 §7）。
    let (severity, context): (String, String) = sqlx::query_as(
        "SELECT severity, context_json FROM run_events WHERE event_kind = 'entry_permanent_failed'",
    )
    .fetch_one(&pool)
    .await
    .expect("entry_permanent_failed event should exist");
    assert_eq!(severity, "error");
    assert!(context.contains(r#""budget_exhausted":true"#));
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

#[tokio::test]
async fn on_failure_retention_keeps_html_only_when_extraction_fails() {
    // D-007: `on_failure` stores the HTML exactly when no strategy extracts the
    // article (permanent failure or summary fallback); a success stores nothing.
    for (strategy, expected_artifacts) in [
        (success_strategy("content-hash-on-failure"), 0_i64),
        (parse_failed_strategy(), 1),
    ] {
        let (_dir, pool) = make_test_pool().await;
        let (_rule_id, source_id) = setup_base(&pool).await;
        let entry_id =
            seed_pending_fetch_entry(&pool, source_id, "uid-on-failure", "hash-on-failure", None)
                .await;
        let flow = flow_with_policy(
            pool.clone(),
            RetentionPolicy::OnFailure,
            responses([(entry_id, Ok(raw(entry_id, b"<html>on failure</html>")))]),
            vec![strategy],
        );

        flow.run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
        let artifact_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts WHERE kind = 'html_payload'")
                .fetch_one(&pool)
                .await
                .expect("artifact count should be readable");
        assert_eq!(artifact_count, expected_artifacts);
    }
}

#[tokio::test]
async fn on_failure_retention_links_html_to_the_summary_fallback_article() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id = seed_pending_fetch_entry(
        &pool,
        source_id,
        "uid-on-failure-fallback",
        "hash-on-failure-fallback",
        Some("<p>summary fallback body for on_failure</p>"),
    )
    .await;
    let flow = flow_with_policy(
        pool.clone(),
        RetentionPolicy::OnFailure,
        responses([(entry_id, Ok(raw(entry_id, b"<html>fallback</html>")))]),
        vec![parse_failed_strategy()],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let artifact_id: i64 = sqlx::query_scalar(
        "SELECT id FROM raw_artifacts WHERE kind = 'html_payload' AND artifact_key = ?",
    )
    .bind(entry_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("the failed extraction keeps its HTML");
    let linked: Option<i64> = sqlx::query_scalar(
        "SELECT a.body_html_artifact_id FROM articles a \
         JOIN feed_entries e ON e.article_id = a.id WHERE e.id = ?",
    )
    .bind(entry_id)
    .fetch_one(&pool)
    .await
    .expect("fallback article");

    assert_eq!(summary.fallback_persisted, 1);
    assert_eq!(linked, Some(artifact_id));
}

#[tokio::test]
async fn on_failure_retention_keeps_html_when_a_strategy_is_retryable() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id =
        seed_pending_fetch_entry(&pool, source_id, "uid-on-failure-retry", "hash-retry", None)
            .await;
    let retryable: Arc<dyn ContentStrategy> = Arc::new(MockStrategy {
        strategy: ExtractorStrategy::Readability,
        extract_fn: Box::new(|_task, _bytes, _final_url| Err(ExtractorError::HttpTimeout)),
    });
    let flow = flow_with_policy(
        pool.clone(),
        RetentionPolicy::OnFailure,
        responses([(entry_id, Ok(raw(entry_id, b"<html>retry</html>")))]),
        vec![retryable],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts WHERE kind = 'html_payload'")
            .fetch_one(&pool)
            .await
            .expect("artifact count should be readable");

    let (state, lease_owner): (String, Option<String>) =
        sqlx::query_as("SELECT state, lease_owner FROM feed_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("entry should be readable");

    assert_eq!(summary.retryable_failed, 1);
    assert_eq!(
        state, "pending_fetch",
        "the retryable entry is queued again"
    );
    assert_eq!(lease_owner, None);
    assert_eq!(artifact_count, 1);
}

#[tokio::test]
async fn always_retention_keeps_html_even_when_a_strategy_panics() {
    // `always` decides before the strategy chain, so a parser panic on
    // untrusted HTML still leaves the payload for replay.
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let entry_id =
        seed_pending_fetch_entry(&pool, source_id, "uid-panic", "hash-panic", None).await;
    let panicking: Arc<dyn ContentStrategy> = Arc::new(MockStrategy {
        strategy: ExtractorStrategy::Readability,
        extract_fn: Box::new(|_task, _bytes, _final_url| panic!("mock strategy panic")),
    });
    let flow = flow_with_policy(
        pool.clone(),
        RetentionPolicy::Always,
        responses([(entry_id, Ok(raw(entry_id, b"<html>panic</html>")))]),
        vec![panicking],
    );

    let summary = flow
        .run(ExtractOptions {
            batch_size: 1,
            max_attempts: 5,
            max_batches: 0,
        })
        .await;
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts WHERE kind = 'html_payload'")
            .fetch_one(&pool)
            .await
            .expect("artifact count should be readable");

    assert_eq!(summary.tasks_panicked, 1);
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
    flow_with_policy(pool, RetentionPolicy::Always, responses, strategies)
}

fn flow_with_policy(
    pool: SqlitePool,
    retention_policy: RetentionPolicy,
    responses: HashMap<i64, Result<RawHtmlFetch, ExtractorError>>,
    strategies: Vec<Arc<dyn ContentStrategy>>,
) -> ExtractFlow {
    let app = Arc::new(app_config(retention_policy, 1));
    let ctx = Arc::new(common::extract_deps(
        pool,
        app,
        Arc::new(MockHtmlFetcher {
            responses: Mutex::new(responses),
        }),
        strategies,
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
    // parse_failed_strategy → PermanentFailed，没有行进入 retry cooldown。
    assert!(!summary.retryable_deferred);

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
    // 自然耗尽（claim 返回空）且没有 retryable 失败。
    assert!(!summary.retryable_deferred);

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

#[tokio::test]
async fn extract_live_tasks_are_bounded_by_concurrency() {
    struct ObserveTasks {
        deps: std::sync::OnceLock<std::sync::Weak<rss_ai_news_runtime::context::ExtractDeps>>,
        peak: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl HtmlFetcher for ObserveTasks {
        async fn fetch_html(&self, _: &ArticleFetchTask) -> Result<RawHtmlFetch, ExtractorError> {
            self.peak.fetch_max(
                self.deps.get().unwrap().strong_count(),
                std::sync::atomic::Ordering::SeqCst,
            );
            tokio::task::yield_now().await;
            Err(ExtractorError::ConnectionFailed)
        }
    }
    let (_dir, pool) = make_test_pool().await;
    let (_, source_id) = setup_base(&pool).await;
    for i in 0..20 {
        seed_pending_fetch_entry(
            &pool,
            source_id,
            &format!("bounded-{i}"),
            &format!("bounded-hash-{i}"),
            None,
        )
        .await;
    }
    let fetcher = Arc::new(ObserveTasks {
        deps: Default::default(),
        peak: Default::default(),
    });
    let app = Arc::new(app_config(RetentionPolicy::Always, 1)); // concurrent_fetches = 2
    let deps = Arc::new(common::extract_deps(pool, app, fetcher.clone(), vec![]));
    fetcher.deps.set(Arc::downgrade(&deps)).unwrap();
    let summary = ExtractFlow::new(deps)
        .run(ExtractOptions {
            batch_size: 20,
            max_attempts: 3,
            max_batches: 1,
        })
        .await;
    assert_eq!(summary.claimed, 20);
    assert_eq!(summary.retryable_failed, 20);
    assert_eq!(summary.tasks_panicked, 0);
    let peak = fetcher.peak.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        peak <= 3,
        "{peak} context owners: only flow + 2 live tasks allowed"
    );
}

// === Retry cooldown: retryable failures no longer stop the run ===

/// Fetcher that records every fetch and can expire another entry's cooldown
/// when a given entry is fetched (simulates a deadline passing mid-run).
struct RecordingFetcher {
    pool: SqlitePool,
    responses: Mutex<HashMap<i64, Result<RawHtmlFetch, ExtractorError>>>,
    calls: Mutex<Vec<i64>>,
    /// `(fetched_id, cooled_id)`: fetching `fetched_id` sets `cooled_id`'s
    /// `next_attempt_at` to the current instant, so its cooldown has expired
    /// before the run's next claim while still lying after run start, as any
    /// deadline written during the run does.
    expire_on_fetch: Option<(i64, i64)>,
}

#[async_trait]
impl HtmlFetcher for RecordingFetcher {
    async fn fetch_html(&self, task: &ArticleFetchTask) -> Result<RawHtmlFetch, ExtractorError> {
        self.calls.lock().await.push(task.feed_entry_id);
        if let Some((fetched, cooled)) = self.expire_on_fetch
            && fetched == task.feed_entry_id
        {
            set_next_attempt_at(&self.pool, cooled, time::OffsetDateTime::now_utc()).await;
        }
        let response = self.responses.lock().await.remove(&task.feed_entry_id);
        response.unwrap_or(Err(ExtractorError::ConnectionFailed))
    }
}

fn recording_flow(
    pool: &SqlitePool,
    responses: HashMap<i64, Result<RawHtmlFetch, ExtractorError>>,
    expire_on_fetch: Option<(i64, i64)>,
) -> (ExtractFlow, Arc<RecordingFetcher>) {
    let fetcher = Arc::new(RecordingFetcher {
        pool: pool.clone(),
        responses: Mutex::new(responses),
        calls: Mutex::default(),
        expire_on_fetch,
    });
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let deps = common::extract_deps(
        pool.clone(),
        app,
        fetcher.clone(),
        vec![per_entry_success_strategy()],
    );
    (ExtractFlow::new(Arc::new(deps)), fetcher)
}

/// Success with a content hash unique to the entry (no cross-entry dedup).
fn per_entry_success_strategy() -> Arc<dyn ContentStrategy> {
    Arc::new(MockStrategy {
        strategy: ExtractorStrategy::Readability,
        extract_fn: Box::new(|task, _bytes, final_url| {
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
        }),
    })
}

fn past() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc() - time::Duration::hours(1)
}

async fn set_next_attempt_at(pool: &SqlitePool, id: i64, at: time::OffsetDateTime) {
    sqlx::query("UPDATE feed_entries SET next_attempt_at = ? WHERE id = ?")
        .bind(at)
        .bind(id)
        .execute(pool)
        .await
        .expect("next_attempt_at should update");
}

async fn entry_row(pool: &SqlitePool, id: i64) -> (String, i64, Option<time::OffsetDateTime>) {
    sqlx::query_as("SELECT state, attempt_count, next_attempt_at FROM feed_entries WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("entry should be readable")
}

fn opts(batch_size: u32, max_batches: u32) -> ExtractOptions {
    ExtractOptions {
        batch_size,
        max_attempts: 5,
        max_batches,
    }
}

#[tokio::test]
async fn timeout_on_first_entry_does_not_block_later_healthy_entry() {
    // Regression for v0.10.3: batch_size=1, the oldest entry times out, and the
    // run used to stop there ("deferring re-claim to next run").
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let slow = seed_pending_fetch_entry(&pool, source_id, "uid-slow", "hash-slow", None).await;
    let healthy = seed_pending_fetch_entry(&pool, source_id, "uid-ok", "hash-ok", None).await;
    // When `healthy` is fetched, `slow`'s cooldown has already passed: the run
    // must still not reclaim `slow`. (SQLite compares these RFC3339 texts
    // lexically; two instants milliseconds apart can misorder in rare cases
    // when the trimmed fraction of one is a prefix of the other. The storage
    // test `retryable_release_sets_deadline_and_claim_honours_it` covers the
    // same cutoff rule with whole-second timestamps.)
    let (flow, fetcher) = recording_flow(
        &pool,
        responses([
            (slow, Err(ExtractorError::HttpTimeout)),
            (healthy, Ok(raw(healthy, b"<html>ok</html>"))),
        ]),
        Some((healthy, slow)),
    );

    let summary = flow.run(opts(1, 0)).await;
    let run_ended = time::OffsetDateTime::now_utc();

    // The cooldown did expire before the run ended, yet `slow` was fetched once.
    let expired_at = entry_row(&pool, slow).await.2.expect("deadline set");
    assert!(expired_at <= run_ended);
    assert_eq!(*fetcher.calls.lock().await, [slow, healthy]);
    assert_eq!(summary.claimed, 2);
    assert_eq!(summary.batches_executed, 2);
    assert_eq!(summary.persisted, 1);
    assert_eq!(summary.retryable_failed, 1);
    assert!(summary.retryable_deferred);
    assert!(!summary.max_batches_reached);
    assert_eq!(entry_row(&pool, healthy).await.0, "persisted");
    let (state, attempts, _) = entry_row(&pool, slow).await;
    assert_eq!((state.as_str(), attempts), ("pending_fetch", 1));
}

#[tokio::test]
async fn retryable_release_records_bounded_cooldown_after_run_start() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let id = seed_pending_fetch_entry(&pool, source_id, "uid-slow", "hash-slow", None).await;
    let (flow, _) = recording_flow(
        &pool,
        responses([(id, Err(ExtractorError::HttpTimeout))]),
        None,
    );

    let before = time::OffsetDateTime::now_utc();
    flow.run(opts(1, 0)).await;
    let after = time::OffsetDateTime::now_utc();

    // First failed attempt: 10-minute cooldown measured from the failure.
    let deadline = entry_row(&pool, id).await.2.expect("cooldown recorded");
    assert!(deadline >= before + time::Duration::minutes(10));
    assert!(deadline <= after + time::Duration::minutes(10));
}

#[tokio::test]
async fn cooling_entry_is_skipped_until_deadline_then_retried_by_later_run() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let id = seed_pending_fetch_entry(&pool, source_id, "uid-cool", "hash-cool", None).await;

    // Run 1: retryable 503 puts the entry into cooldown.
    let (flow, _) = recording_flow(
        &pool,
        responses([(id, Err(ExtractorError::HttpStatus { code: 503 }))]),
        None,
    );
    assert_eq!(flow.run(opts(1, 0)).await.retryable_failed, 1);
    let (_, _, deadline) = entry_row(&pool, id).await;
    assert!(deadline.expect("cooldown recorded") > time::OffsetDateTime::now_utc());

    // Run 2, before the deadline: not claimed, not fetched.
    let (flow, fetcher) = recording_flow(&pool, responses([(id, Ok(raw(id, b"x")))]), None);
    let summary = flow.run(opts(1, 0)).await;
    assert_eq!((summary.claimed, summary.batches_executed), (0, 0));
    assert!(fetcher.calls.lock().await.is_empty());

    // Run 3, after the deadline (explicit timestamp, no sleep): retried.
    set_next_attempt_at(&pool, id, past()).await;
    let (flow, _) = recording_flow(&pool, responses([(id, Ok(raw(id, b"x")))]), None);
    let summary = flow.run(opts(1, 0)).await;
    assert_eq!((summary.claimed, summary.persisted), (1, 1));
    let (state, attempts, _) = entry_row(&pool, id).await;
    assert_eq!((state.as_str(), attempts), ("persisted", 2));
}

#[tokio::test]
async fn queue_of_only_cooling_entries_exits_without_spinning() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    for i in 0..3 {
        let id = seed_pending_fetch_entry(
            &pool,
            source_id,
            &format!("uid-c{i}"),
            &format!("hash-c{i}"),
            None,
        )
        .await;
        set_next_attempt_at(&pool, id, future).await;
    }
    let (flow, fetcher) = recording_flow(&pool, HashMap::new(), None);

    // Unlimited batches: only an empty claim can end the loop.
    let summary = flow.run(opts(1, 0)).await;

    assert_eq!((summary.claimed, summary.batches_executed), (0, 0));
    assert!(!summary.max_batches_reached && !summary.retryable_deferred);
    assert!(fetcher.calls.lock().await.is_empty());
}

#[tokio::test]
async fn max_batches_still_caps_a_run_with_retryable_failures() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let mut ids = Vec::new();
    for i in 0..3 {
        let uid = format!("uid-t{i}");
        let hash = format!("hash-t{i}");
        ids.push(seed_pending_fetch_entry(&pool, source_id, &uid, &hash, None).await);
    }
    // No responses: every fetch fails with retryable ConnectionFailed.
    let (flow, fetcher) = recording_flow(&pool, HashMap::new(), None);

    let summary = flow.run(opts(1, 2)).await;

    assert_eq!(*fetcher.calls.lock().await, ids[..2]);
    assert_eq!((summary.claimed, summary.batches_executed), (2, 2));
    assert_eq!(summary.retryable_failed, 2);
    assert!(summary.max_batches_reached);
    assert!(summary.retryable_deferred);
    assert_eq!(entry_row(&pool, ids[2]).await.1, 0, "third entry untouched");
}

/// Known limitation, not a requirement: the claim is still oldest-first, so an
/// eligible backlog larger than `batch_size * max_batches` keeps a fresh entry
/// out of the run. The cooldown fix does not change this. Invert this test if
/// a bounded fresh-first claim policy is adopted.
#[tokio::test]
async fn oldest_first_backlog_still_starves_fresh_entries_under_cap() {
    let (_dir, pool) = make_test_pool().await;
    let (_rule_id, source_id) = setup_base(&pool).await;
    let old_discovered = time::OffsetDateTime::now_utc() - time::Duration::hours(72);
    let mut queue = HashMap::new();
    for i in 0..3 {
        let uid = format!("uid-o{i}");
        let hash = format!("hash-o{i}");
        let id = seed_pending_fetch_entry(&pool, source_id, &uid, &hash, None).await;
        sqlx::query("UPDATE feed_entries SET discovered_at = ? WHERE id = ?")
            .bind(old_discovered + time::Duration::minutes(i))
            .bind(id)
            .execute(&pool)
            .await
            .expect("discovered_at should update");
        queue.insert(id, Ok(raw(id, b"old")));
    }
    let fresh = seed_pending_fetch_entry(&pool, source_id, "uid-fresh", "hash-fresh", None).await;
    queue.insert(fresh, Ok(raw(fresh, b"fresh")));
    let (flow, fetcher) = recording_flow(&pool, queue, None);

    let summary = flow.run(opts(1, 2)).await;

    assert_eq!(summary.persisted, 2);
    assert!(summary.max_batches_reached);
    assert!(!fetcher.calls.lock().await.contains(&fresh));
    assert_eq!(entry_row(&pool, fresh).await.0, "pending_fetch");
}
