mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rss_ai_news_config::{RetentionPolicy, SourceSecrets};
use rss_ai_news_domain::SecretString;
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_domain::link_normalizer::normalize_link;
use rss_ai_news_feed::fetcher::RawFeedFetch;
use rss_ai_news_feed::{FeedError, FeedFetcher};
use rss_ai_news_runtime::{IngestFlow, IngestOptions, IngestSourceStatus};
use rss_ai_news_storage::{FeedEntryRepo, FeedEntryRepository, NewFeedEntry};
use sqlx::SqlitePool;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use common::{
    app_config, category_with_sources, full_context, insert_config_rule, insert_source,
    make_test_pool, make_test_pool_with_connections,
};

const RSS_BODY: &[u8] = include_bytes!("../../feed/tests/fixtures/rss_2.0_minimal.xml");

struct MockFeedFetcher {
    responses: Mutex<HashMap<i64, MockResponse>>,
}

struct MockResponse {
    delay: Duration,
    result: Result<RawFeedFetch, FeedError>,
}

#[async_trait]
impl FeedFetcher for MockFeedFetcher {
    async fn fetch_raw(&self, req: &FeedFetchRequest) -> Result<RawFeedFetch, FeedError> {
        let response = {
            let mut guard = self.responses.lock().await;
            guard.remove(&req.source_id)
        };
        let Some(response) = response else {
            return Err(FeedError::ConnectionFailed {
                source: "missing mock response".to_string(),
            });
        };
        if !response.delay.is_zero() {
            tokio::time::sleep(response.delay).await;
        }
        response.result
    }
}

#[tokio::test]
async fn single_source_200_inserts_all_entries() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_config_rule(&pool).await;
    let source_id = insert_source(&pool, config_id, "s1", "https://example.com/s1.xml").await;
    let flow = flow(
        pool.clone(),
        RetentionPolicy::Always,
        2,
        category_with_sources(&["s1"]),
        responses([(source_id, ok_payload(source_id, RSS_BODY))]),
    );

    let summary = flow.run(IngestOptions::default()).await;
    let source_row: (Option<OffsetDateTime>, i64) = sqlx::query_as(
        "SELECT last_success_at, consecutive_failures FROM feed_sources WHERE id = ?",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .expect("source should be readable");
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts WHERE kind = 'feed_payload'")
            .fetch_one(&pool)
            .await
            .expect("artifact count should be readable");

    assert_eq!(summary.entries_inserted, 3);
    assert_eq!(summary.sources_succeeded, 1);
    assert!(source_row.0.is_some());
    assert_eq!(source_row.1, 0);
    assert_eq!(artifact_count, 1);
}

#[tokio::test]
async fn single_source_304_marks_not_modified_no_entries() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_config_rule(&pool).await;
    let source_id = insert_source(&pool, config_id, "s1", "https://example.com/s1.xml").await;
    let flow = flow(
        pool.clone(),
        RetentionPolicy::Always,
        2,
        category_with_sources(&["s1"]),
        responses([(source_id, not_modified(source_id))]),
    );

    let summary = flow.run(IngestOptions::default()).await;
    let last_success_at: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT last_success_at FROM feed_sources WHERE id = ?")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .expect("source should be readable");

    assert_eq!(summary.entries_inserted, 0);
    assert_eq!(summary.sources_not_modified, 1);
    assert!(last_success_at.is_some());
}

#[tokio::test]
async fn existing_source_is_synced_from_current_config_before_fetch() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_config_rule(&pool).await;
    let source_id = insert_source(&pool, config_id, "s1", "{RSSHUB}/s1.xml").await;
    sqlx::query(
        "UPDATE feed_sources SET consecutive_failures = 3, last_error = 'Feed URL 无效', last_error_kind = 'invalid_url' WHERE id = ?",
    )
    .bind(source_id)
    .execute(&pool)
    .await
    .expect("seed stale source error");

    let mut category = category_with_sources(&["s1"]);
    category.sources[0].feed_url = "http://rsshub:1200/s1.xml".to_string();
    let mut source_secrets = SourceSecrets::default();
    source_secrets.insert_rsshub_access_key("ai", "s1", SecretString::new("test-key"));
    let flow = flow_with_source_secrets(
        pool.clone(),
        RetentionPolicy::Always,
        2,
        category,
        responses([(source_id, ok_payload(source_id, RSS_BODY))]),
        source_secrets,
    );

    let summary = flow.run(IngestOptions::default()).await;
    let source_row: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT feed_url, consecutive_failures, last_error_kind FROM feed_sources WHERE id = ?",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .expect("source should be readable");

    assert_eq!(summary.sources_succeeded, 1);
    assert_eq!(source_row.0, "http://rsshub:1200/s1.xml");
    assert_eq!(source_row.1, 0);
    assert_eq!(source_row.2, None);
}

#[tokio::test]
async fn single_source_5xx_marks_failed_writes_event() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_config_rule(&pool).await;
    let source_id = insert_source(&pool, config_id, "s1", "https://example.com/s1.xml").await;
    let flow = flow(
        pool.clone(),
        RetentionPolicy::Always,
        2,
        category_with_sources(&["s1"]),
        responses([(
            source_id,
            MockResponse {
                delay: Duration::ZERO,
                result: Err(FeedError::HttpStatus { code: 503 }),
            },
        )]),
    );

    let summary = flow.run(IngestOptions::default()).await;
    let failures: i64 =
        sqlx::query_scalar("SELECT consecutive_failures FROM feed_sources WHERE id = ?")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .expect("source should be readable");
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_events WHERE event_kind = 'source_fetch_failed'",
    )
    .fetch_one(&pool)
    .await
    .expect("event count should be readable");

    assert_eq!(summary.per_source[0].status, IngestSourceStatus::Failed);
    assert_eq!(failures, 1);
    assert_eq!(event_count, 1);
}

#[tokio::test]
async fn link_hash_dup_skipped_aggregated_event() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_config_rule(&pool).await;
    let source_id = insert_source(&pool, config_id, "s1", "https://example.com/s1.xml").await;
    let link = normalize_link("https://example.com/rss/1").expect("link should normalize");
    FeedEntryRepo::new(pool.clone())
        .insert_if_new(&new_entry(
            source_id,
            "existing-uid",
            &link.normalized,
            &link.link_hash,
        ))
        .await
        .expect("seed entry should insert");
    let flow = flow(
        pool.clone(),
        RetentionPolicy::Always,
        2,
        category_with_sources(&["s1"]),
        responses([(
            source_id,
            ok_payload(
                source_id,
                single_item("rss-new", "https://example.com/rss/1").as_bytes(),
            ),
        )]),
    );

    let summary = flow.run(IngestOptions::default()).await;
    let context_json: String = sqlx::query_scalar(
        "SELECT context_json FROM run_events WHERE event_kind = 'entry_dedup_skipped'",
    )
    .fetch_one(&pool)
    .await
    .expect("dedup event should exist");

    assert_eq!(summary.entries_inserted, 0);
    assert_eq!(summary.entries_link_dup, 1);
    assert!(context_json.contains("link_dup"));
}

#[tokio::test]
async fn uid_dup_skipped_aggregated_event() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_config_rule(&pool).await;
    let source_id = insert_source(&pool, config_id, "s1", "https://example.com/s1.xml").await;
    let seeded_link = normalize_link("https://example.com/seed").expect("link should normalize");
    FeedEntryRepo::new(pool.clone())
        .insert_if_new(&new_entry(
            source_id,
            "rss-1",
            &seeded_link.normalized,
            &seeded_link.link_hash,
        ))
        .await
        .expect("seed entry should insert");
    let flow = flow(
        pool.clone(),
        RetentionPolicy::Always,
        2,
        category_with_sources(&["s1"]),
        responses([(
            source_id,
            ok_payload(
                source_id,
                single_item("rss-1", "https://example.com/rss/uid-new").as_bytes(),
            ),
        )]),
    );

    let summary = flow.run(IngestOptions::default()).await;
    let context_json: String = sqlx::query_scalar(
        "SELECT context_json FROM run_events WHERE event_kind = 'entry_dedup_skipped'",
    )
    .fetch_one(&pool)
    .await
    .expect("dedup event should exist");

    assert_eq!(summary.entries_inserted, 0);
    assert_eq!(summary.entries_uid_dup, 1);
    assert!(context_json.contains("uid_dup"));
}

#[tokio::test]
async fn parse_failure_keeps_artifact_marks_failed() {
    let (_dir, pool) = make_test_pool().await;
    let config_id = insert_config_rule(&pool).await;
    let source_id = insert_source(&pool, config_id, "s1", "https://example.com/s1.xml").await;
    let flow = flow(
        pool.clone(),
        RetentionPolicy::Always,
        2,
        category_with_sources(&["s1"]),
        responses([(source_id, ok_payload(source_id, b"<<not xml>>"))]),
    );

    let summary = flow.run(IngestOptions::default()).await;
    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts WHERE kind = 'feed_payload'")
            .fetch_one(&pool)
            .await
            .expect("artifact count should be readable");
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_events WHERE event_kind = 'source_fetch_failed'",
    )
    .fetch_one(&pool)
    .await
    .expect("event count should be readable");

    assert_eq!(summary.per_source[0].status, IngestSourceStatus::Failed);
    assert_eq!(artifact_count, 1);
    assert_eq!(event_count, 1);
}

#[tokio::test]
async fn multi_source_concurrent_within_limit() {
    let (_dir, pool) = make_test_pool_with_connections(8).await;
    let config_id = insert_config_rule(&pool).await;
    let keys = ["s1", "s2", "s3", "s4", "s5"];
    let mut response_items = Vec::new();
    for key in keys {
        let source_id = insert_source(
            &pool,
            config_id,
            key,
            &format!("https://example.com/{key}.xml"),
        )
        .await;
        response_items.push((
            source_id,
            MockResponse {
                delay: Duration::from_millis(100),
                result: Ok(raw(
                    source_id,
                    single_item(
                        &format!("{key}-uid"),
                        &format!("https://example.com/{key}/1"),
                    )
                    .into_bytes(),
                )),
            },
        ));
    }
    let flow = flow(
        pool,
        RetentionPolicy::Always,
        2,
        category_with_sources(&keys),
        responses(response_items),
    );

    let started = Instant::now();
    let summary = flow.run(IngestOptions::default()).await;
    let elapsed = started.elapsed();

    assert_eq!(summary.sources_succeeded, 5);
    assert_eq!(summary.entries_inserted, 5);
    assert!(elapsed < Duration::from_millis(800), "elapsed: {elapsed:?}");
}

#[tokio::test]
async fn ingest_bootstrap_writes_config_kind_id_into_feed_sources_config_version() {
    // F15-fix6：与 F15-fix3 同源——`feed_sources.config_version` 必须指向
    // `kind='config'` 的 rule_versions 行，**不**得指向硬编码 id=1 的非
    // config 行。本测试不预插任何 feed_source 与 `insert_config_rule`，
    // 强制 resolve_source 走 bootstrap：upsert 新 feed_source 时调
    // `active_rule_or_register("config", ...)` seed 出一行 kind='config'
    // 的 placeholder（tag='ingest-bootstrap'）；feed_sources.config_version
    // 反查 rule_versions.kind 必须 == 'config'。
    //
    // fetcher 用空 responses → 任何 source_id 都返回 ConnectionFailed，
    // 但那是 resolve_source 之后才发生；feed_sources 行已先落地。
    let (_dir, pool) = make_test_pool().await;

    let flow = flow(
        pool.clone(),
        RetentionPolicy::Always,
        2,
        category_with_sources(&["s-bootstrap"]),
        HashMap::new(),
    );

    let _summary = flow.run(IngestOptions::default()).await;

    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT rv.kind FROM feed_sources fs
         JOIN rule_versions rv ON rv.id = fs.config_version
         ORDER BY fs.id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        !kinds.is_empty(),
        "Ingest bootstrap 应当至少 upsert 一行 feed_sources"
    );
    for kind in &kinds {
        assert_eq!(
            kind, "config",
            "feed_sources.config_version 必须指向 kind='config' 行，实际：{kind}"
        );
    }
}

fn flow(
    pool: SqlitePool,
    retention_policy: RetentionPolicy,
    concurrent_feeds: u32,
    category: rss_ai_news_config::CategoryConfig,
    responses: HashMap<i64, MockResponse>,
) -> IngestFlow {
    flow_with_source_secrets(
        pool,
        retention_policy,
        concurrent_feeds,
        category,
        responses,
        SourceSecrets::default(),
    )
}

fn flow_with_source_secrets(
    pool: SqlitePool,
    retention_policy: RetentionPolicy,
    concurrent_feeds: u32,
    category: rss_ai_news_config::CategoryConfig,
    responses: HashMap<i64, MockResponse>,
    source_secrets: SourceSecrets,
) -> IngestFlow {
    let app = Arc::new(app_config(retention_policy, concurrent_feeds));
    let fetcher = Arc::new(MockFeedFetcher {
        responses: Mutex::new(responses),
    });
    let ctx = Arc::new(full_context("ingest", pool, app, fetcher));
    IngestFlow::with_source_secrets(ctx, vec![category], source_secrets)
}

fn responses(items: impl IntoIterator<Item = (i64, MockResponse)>) -> HashMap<i64, MockResponse> {
    items.into_iter().collect()
}

fn ok_payload(source_id: i64, body: &[u8]) -> MockResponse {
    MockResponse {
        delay: Duration::ZERO,
        result: Ok(raw(source_id, body.to_vec())),
    }
}

fn raw(source_id: i64, body: Vec<u8>) -> RawFeedFetch {
    RawFeedFetch {
        source_id,
        http_status: 200,
        etag: Some(format!("etag-{source_id}")),
        last_modified: Some("Wed, 01 Jan 2025 00:00:00 GMT".to_string()),
        not_modified: false,
        raw_payload_bytes: Some(body),
    }
}

fn not_modified(source_id: i64) -> MockResponse {
    MockResponse {
        delay: Duration::ZERO,
        result: Ok(RawFeedFetch {
            source_id,
            http_status: 304,
            etag: Some(format!("etag-{source_id}")),
            last_modified: None,
            not_modified: true,
            raw_payload_bytes: None,
        }),
    }
}

fn new_entry(source_id: i64, uid: &str, normalized_link: &str, link_hash: &str) -> NewFeedEntry {
    NewFeedEntry {
        source_id,
        feed_entry_uid: uid.to_string(),
        normalized_link: normalized_link.to_string(),
        link_hash: link_hash.to_string(),
        title_raw: "seed".to_string(),
        summary_raw: None,
        published_at: None,
        discovered_at: OffsetDateTime::now_utc(),
    }
}

fn single_item(uid: &str, link: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Example RSS</title>
    <link>https://example.com/</link>
    <description>Example RSS feed</description>
    <item>
      <guid>{uid}</guid>
      <title>{uid}</title>
      <link>{link}</link>
      <description>summary</description>
      <pubDate>Wed, 01 Jan 2025 00:00:00 GMT</pubDate>
    </item>
  </channel>
</rss>"#
    )
}
