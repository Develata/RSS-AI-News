use std::time::Duration;

use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_domain::state::FeedKind;
use rss_ai_news_feed::{FeedError, FeedFetcher, ReqwestFeedFetcher};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RSS_BODY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Example RSS</title>
    <link>https://example.com/</link>
    <description>Example RSS feed</description>
    <item>
      <guid>rss-1</guid>
      <title>RSS item 1</title>
      <link>https://example.com/rss/1</link>
      <description>RSS summary 1</description>
      <pubDate>Wed, 01 Jan 2025 00:00:00 GMT</pubDate>
    </item>
  </channel>
</rss>"#;

fn request(server: &MockServer, path: &str) -> FeedFetchRequest {
    FeedFetchRequest {
        source_id: 42,
        category_key: "ai".to_string(),
        source_key: "example".to_string(),
        feed_url: format!("{}{}", server.uri(), path),
        feed_kind: FeedKind::Rss,
        etag: None,
        last_modified: None,
        timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn fetches_200_ok_rss_payload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "application/rss+xml")
                .insert_header("ETag", "\"abc\"")
                .insert_header("Last-Modified", "Wed, 01 Jan 2025 00:00:00 GMT")
                .set_body_string(RSS_BODY),
        )
        .mount(&server)
        .await;

    let fetcher = ReqwestFeedFetcher::new(1024 * 1024).expect("fetcher should build");
    let response = fetcher
        .fetch(&request(&server, "/feed"))
        .await
        .expect("fetch should succeed");

    assert!(!response.entries.is_empty());
    assert_eq!(response.etag.as_deref(), Some("\"abc\""));
    assert_eq!(
        response.last_modified.as_deref(),
        Some("Wed, 01 Jan 2025 00:00:00 GMT")
    );
    assert!(!response.not_modified);
    assert!(response.raw_payload_bytes.is_some());
}

#[tokio::test]
async fn returns_304_not_modified_without_payload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .and(header("If-None-Match", "\"abc\""))
        .respond_with(ResponseTemplate::new(304).insert_header("ETag", "\"abc\""))
        .mount(&server)
        .await;

    let fetcher = ReqwestFeedFetcher::new(1024 * 1024).expect("fetcher should build");
    let mut req = request(&server, "/feed");
    req.etag = Some("\"abc\"".to_string());
    let response = fetcher.fetch(&req).await.expect("fetch should succeed");

    assert!(response.not_modified);
    assert!(response.entries.is_empty());
    assert!(response.raw_payload_bytes.is_none());
    assert_eq!(response.http_status, 304);
}

#[tokio::test]
async fn returns_404_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let fetcher = ReqwestFeedFetcher::new(1024 * 1024).expect("fetcher should build");
    let err = fetcher
        .fetch(&request(&server, "/missing"))
        .await
        .expect_err("404 should fail");

    assert_eq!(err, FeedError::HttpStatus { code: 404 });
}

#[tokio::test]
async fn returns_500_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/error"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let fetcher = ReqwestFeedFetcher::new(1024 * 1024).expect("fetcher should build");
    let err = fetcher
        .fetch(&request(&server, "/error"))
        .await
        .expect_err("500 should fail");

    assert_eq!(err, FeedError::HttpStatus { code: 500 });
}

#[tokio::test]
async fn maps_timeout_to_http_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(300))
                .set_body_string(RSS_BODY),
        )
        .mount(&server)
        .await;

    let fetcher = ReqwestFeedFetcher::new(1024 * 1024).expect("fetcher should build");
    let mut req = request(&server, "/slow");
    req.timeout = Duration::from_millis(100);
    let err = fetcher.fetch(&req).await.expect_err("timeout should fail");

    assert_eq!(err, FeedError::HttpTimeout);
}

#[tokio::test]
async fn rejects_too_large_response() {
    let server = MockServer::start().await;
    let body = "x".repeat(2048);
    Mock::given(method("GET"))
        .and(path("/large"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let fetcher = ReqwestFeedFetcher::new(1024).expect("fetcher should build");
    let err = fetcher
        .fetch(&request(&server, "/large"))
        .await
        .expect_err("large response should fail");

    assert!(matches!(err, FeedError::TooLarge { bytes } if bytes > 1024));
}
