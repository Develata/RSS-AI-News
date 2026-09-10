//! Deterministic local measurements. Run through tools/perf/run.py; no live APIs.
mod common;

use async_trait::async_trait;
use rss_ai_news_ai::{AiClient, AiError, AiResponse, AiTask};
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::{
    Score0To100,
    dto::{extract::ArticleFetchTask, feed::FeedFetchRequest, publish::FrozenPublishItem},
    state::FeedKind,
};
use rss_ai_news_extractor::{ContentStrategy, ReadabilityStrategy};
use rss_ai_news_feed::fetcher::RawFeedFetch;
use rss_ai_news_feed::{FeedError, FeedFetcher, parse_feed};
use rss_ai_news_report::{RenderConfig, RenderTemplates, render_markdown};
use rss_ai_news_runtime::{AiRunFlow, AiRunOptions, IngestFlow, IngestOptions};
use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};
use time::OffsetDateTime;

fn measurement(case: &str, items: usize, started: Instant) {
    println!(
        "PERF {}",
        serde_json::json!({"case": case, "items": items, "elapsed_ns": started.elapsed().as_nanos()})
    );
}

fn feed_fixture(items: usize) -> Vec<u8> {
    let mut xml = String::from(
        "<rss version=\"2.0\"><channel><title>Fixture</title><link>https://example.invalid/</link><description>deterministic</description>",
    );
    for i in 0..items {
        xml.push_str(&format!("<item><guid>entry-{i}</guid><title>Entry {i}</title><link>https://example.invalid/{i}</link><description>Local fixture summary {i}</description></item>"));
    }
    xml.push_str("</channel></rss>");
    xml.into_bytes()
}

#[test]
#[ignore = "manual deterministic performance measurement"]
fn feed_parse() {
    let fixture = feed_fixture(100);
    for _ in 0..10 {
        black_box(parse_feed(&fixture, FeedKind::Rss).unwrap());
    }
    let started = Instant::now();
    for _ in 0..1000 {
        assert_eq!(
            black_box(parse_feed(&fixture, FeedKind::Rss).unwrap()).len(),
            100
        );
    }
    measurement("feed_parse", 100_000, started);
}

struct FixtureFeed(Vec<u8>);
#[async_trait]
impl FeedFetcher for FixtureFeed {
    async fn fetch_raw(&self, request: &FeedFetchRequest) -> Result<RawFeedFetch, FeedError> {
        Ok(RawFeedFetch {
            source_id: request.source_id,
            http_status: 200,
            raw_payload_bytes: Some(self.0.clone()),
            etag: None,
            last_modified: None,
            not_modified: false,
        })
    }
}

#[tokio::test]
#[ignore = "manual deterministic performance measurement"]
async fn feed_ingest() {
    for size in [10, 100, 1000] {
        let dir = tempfile::tempdir().unwrap();
        let pool = rss_ai_news_storage::build_sqlite_pool(&dir.path().join("perf.db"), 4, 5000)
            .await
            .unwrap();
        rss_ai_news_storage::run_migrations(&rss_ai_news_storage::StoragePool::Sqlite(
            pool.clone(),
        ))
        .await
        .unwrap();
        let ctx = common::ingest_deps(
            pool.clone(),
            Arc::new(common::app_config(RetentionPolicy::Off, 1)),
            Arc::new(FixtureFeed(feed_fixture(size))),
        );
        let flow = IngestFlow::new(
            Arc::new(ctx),
            vec![common::category_with_sources(&["fixture"])],
        );
        let started = Instant::now();
        let summary = flow.run(IngestOptions::default()).await;
        assert_eq!(summary.entries_inserted as usize, size);
        assert_eq!(summary.sources_failed, 0);
        measurement(&format!("feed_ingest_{size}"), size, started);
        let started = Instant::now();
        let summary = flow.run(IngestOptions::default()).await;
        assert_eq!(summary.entries_inserted, 0);
        assert_eq!(summary.sources_failed, 0);
        measurement(&format!("feed_dedup_{size}"), size, started);
        drop(flow);
        pool.close().await;
    }
}

#[test]
#[ignore = "manual deterministic performance measurement"]
fn report_render() {
    let items: Vec<_> = (0..30)
        .map(|i| {
            FrozenPublishItem::try_new(
                i + 1,
                i64::from(i + 1),
                Some(i64::from(i + 1)),
                format!("Fixture article {i}"),
                "Summary with reproducible text. ".repeat(30),
                "[\"rust\",\"science\"]".into(),
                Some(Score0To100::try_new(80).unwrap()),
                format!("https://example.invalid/{i}"),
                "Fixture source".into(),
            )
            .unwrap()
        })
        .collect();
    let config = RenderConfig {
        category_display_name: "Fixture".into(),
        report_title: "Fixture report".into(),
        generated_at: OffsetDateTime::UNIX_EPOCH,
        templates: RenderTemplates::default(),
    };
    for _ in 0..10 {
        black_box(render_markdown(1, "fixture", "2026-01-01", &items, &config).unwrap());
    }
    let started = Instant::now();
    for _ in 0..1000 {
        black_box(render_markdown(1, "fixture", "2026-01-01", &items, &config).unwrap());
    }
    measurement("report_render", 30_000, started);
}

#[test]
#[ignore = "manual deterministic performance measurement"]
fn extract_fixture() {
    let html = include_bytes!("../../extractor/tests/fixtures/simple_article.html");
    let task = ArticleFetchTask {
        feed_entry_id: 1,
        normalized_link: "https://example.invalid/article".into(),
        title_raw: "Fixture".into(),
        summary_raw: None,
        timeout: Duration::from_secs(1),
    };
    for _ in 0..10 {
        black_box(
            ReadabilityStrategy
                .extract(&task, html, &task.normalized_link)
                .unwrap(),
        );
    }
    let started = Instant::now();
    for _ in 0..1000 {
        black_box(
            ReadabilityStrategy
                .extract(&task, html, &task.normalized_link)
                .unwrap(),
        );
    }
    measurement("extract_fixture", 1000, started);
}

struct FixtureAi;
#[async_trait]
impl AiClient for FixtureAi {
    async fn invoke(&self, task: &AiTask) -> Result<AiResponse, AiError> {
        Ok(AiResponse { article_ai_result_id: task.article_ai_result_id, raw_response: r#"{"summary":"Fixture summary","tags":["fixture"],"importance_score":80,"keep_decision":true}"#.into(), usage: None, latency_ms: 0 })
    }
}

#[tokio::test]
#[ignore = "manual deterministic performance measurement"]
async fn ai_orchestration() {
    let dir = tempfile::tempdir().unwrap();
    let pool = rss_ai_news_storage::build_sqlite_pool(&dir.path().join("perf.db"), 4, 5000)
        .await
        .unwrap();
    rss_ai_news_storage::run_migrations(&rss_ai_news_storage::StoragePool::Sqlite(pool.clone()))
        .await
        .unwrap();
    for i in 0..100 {
        common::seed_persisted_article(
            &pool,
            &format!("perf-{i}"),
            "Fixture title",
            "Fixture body",
        )
        .await;
    }
    let ctx = common::ai_deps(
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Off, 1)),
        Arc::new(FixtureAi),
    );
    let flow = AiRunFlow::new(Arc::new(ctx));
    let opts = AiRunOptions {
        task_gen_batch_size: 100,
        process_batch_size: 10,
        max_attempts: 3,
        prompt_template: "Summarize {body_text}".repeat(1000).into(),
        model_id: "fixture".into(),
        fallback_models: vec!["fallback".into()],
        max_input_chars: 1024,
        max_tokens: 128,
        temperature: 0.0,
        min_importance_score: Score0To100::try_new(30).unwrap(),
        max_batches: 0,
        category_key: "ai".into(),
        prompt_version: 1,
        output_schema_version: 1,
    };
    let started = Instant::now();
    let summary = flow.run(opts).await;
    assert_eq!(summary.task_gen.inserted, 100);
    assert_eq!(summary.process.succeeded, 100);
    assert_eq!(summary.process.tasks_panicked, 0);
    measurement("ai_orchestration", 100, started);
    drop(flow);
    pool.close().await;
}

struct NotModifiedFeed;
#[async_trait]
impl FeedFetcher for NotModifiedFeed {
    async fn fetch_raw(&self, request: &FeedFetchRequest) -> Result<RawFeedFetch, FeedError> {
        Ok(RawFeedFetch {
            source_id: request.source_id,
            http_status: 304,
            raw_payload_bytes: None,
            etag: None,
            last_modified: None,
            not_modified: true,
        })
    }
}

#[tokio::test]
#[ignore = "manual deterministic performance measurement"]
async fn ingest_sources() {
    let dir = tempfile::tempdir().unwrap();
    let pool = rss_ai_news_storage::build_sqlite_pool(&dir.path().join("perf.db"), 4, 5000)
        .await
        .unwrap();
    rss_ai_news_storage::run_migrations(&rss_ai_news_storage::StoragePool::Sqlite(pool.clone()))
        .await
        .unwrap();
    let keys: Vec<_> = (0..1000).map(|i| format!("source-{i}")).collect();
    let refs: Vec<_> = keys.iter().map(String::as_str).collect();
    let category = common::category_with_sources(&refs);
    let ctx = common::ingest_deps(
        pool.clone(),
        Arc::new(common::app_config(RetentionPolicy::Off, 4)),
        Arc::new(NotModifiedFeed),
    );
    let flow = IngestFlow::new(Arc::new(ctx), vec![category]);
    let started = Instant::now();
    let summary = flow.run(IngestOptions::default()).await;
    assert_eq!(summary.sources_not_modified, 1000);
    assert_eq!(summary.sources_failed, 0);
    measurement("ingest_sources", 1000, started);
    drop(flow);
    pool.close().await;
}
