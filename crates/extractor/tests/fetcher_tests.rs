use std::time::Duration;

use rss_ai_news_extractor::{ArticleFetchTask, ExtractorError, HtmlFetcher, ReqwestHtmlFetcher};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HTML_BODY: &str = "<!doctype html><html><body><article>Hello</article></body></html>";

fn task(server: &MockServer, path: &str) -> ArticleFetchTask {
    ArticleFetchTask {
        feed_entry_id: 42,
        normalized_link: format!("{}{}", server.uri(), path),
        title_raw: "Example".to_string(),
        summary_raw: None,
        timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn fetches_200_returns_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/article"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html; charset=utf-8")
                .set_body_string(HTML_BODY),
        )
        .mount(&server)
        .await;

    let fetcher = ReqwestHtmlFetcher::new(1024 * 1024).expect("fetcher should build");
    let response = fetcher
        .fetch_html(&task(&server, "/article"))
        .await
        .expect("HTML fetch should succeed");

    assert_eq!(response.feed_entry_id, 42);
    assert_eq!(response.http_status, 200);
    assert_eq!(response.body_bytes, HTML_BODY.as_bytes());
    assert_eq!(response.final_url, format!("{}/article", server.uri()));
}

#[tokio::test]
async fn returns_http_status_for_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let fetcher = ReqwestHtmlFetcher::new(1024 * 1024).expect("fetcher should build");
    let err = fetcher
        .fetch_html(&task(&server, "/missing"))
        .await
        .expect_err("404 should fail");

    assert_eq!(err, ExtractorError::HttpStatus { code: 404 });
}

#[tokio::test]
async fn returns_http_status_for_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/error"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let fetcher = ReqwestHtmlFetcher::new(1024 * 1024).expect("fetcher should build");
    let err = fetcher
        .fetch_html(&task(&server, "/error"))
        .await
        .expect_err("503 should fail");

    assert_eq!(err, ExtractorError::HttpStatus { code: 503 });
}

#[tokio::test]
async fn returns_too_large_when_body_exceeds_limit() {
    let server = MockServer::start().await;
    let body = "x".repeat(1024 * 1024);
    Mock::given(method("GET"))
        .and(path("/large"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let fetcher = ReqwestHtmlFetcher::new(1024).expect("fetcher should build");
    let err = fetcher
        .fetch_html(&task(&server, "/large"))
        .await
        .expect_err("large body should fail");

    assert!(matches!(err, ExtractorError::TooLarge { bytes } if bytes > 1024));
}

#[tokio::test]
async fn returns_timeout_on_slow_server() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_string(HTML_BODY),
        )
        .mount(&server)
        .await;

    let fetcher = ReqwestHtmlFetcher::new(1024 * 1024).expect("fetcher should build");
    let mut slow_task = task(&server, "/slow");
    slow_task.timeout = Duration::from_millis(200);
    let err = fetcher
        .fetch_html(&slow_task)
        .await
        .expect_err("slow response should time out");

    assert_eq!(err, ExtractorError::HttpTimeout);
}
