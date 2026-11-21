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

#[tokio::test]
async fn body_at_limit_is_preserved_exactly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(HTML_BODY))
        .mount(&server)
        .await;
    let fetcher = ReqwestHtmlFetcher::new(HTML_BODY.len() as u64).expect("fetcher");
    let response = fetcher
        .fetch_html(&task(&server, "/article"))
        .await
        .expect("exact limit succeeds");
    assert_eq!(response.body_bytes, HTML_BODY.as_bytes());
}

// === R2-S10: 429 carries the server's Retry-After ===

async fn status_429(retry_after: Option<&str>) -> ExtractorError {
    let server = MockServer::start().await;
    let mut response = ResponseTemplate::new(429);
    if let Some(value) = retry_after {
        response = response.insert_header("Retry-After", value);
    }
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(response)
        .mount(&server)
        .await;
    ReqwestHtmlFetcher::new(1024)
        .unwrap()
        .fetch_html(&task(&server, "/limited"))
        .await
        .expect_err("429")
}

/// The absolute retry instant, as a delay from `before` (the instant just
/// before the request was sent).
fn delay_from(error: &ExtractorError, before: std::time::SystemTime) -> Option<Duration> {
    error
        .retry_at()
        .map(|at| at.duration_since(before).unwrap_or(Duration::ZERO))
}

#[tokio::test]
async fn retry_after_seconds_on_429_are_kept() {
    let before = std::time::SystemTime::now();
    let error = status_429(Some("3600")).await;
    let delay = delay_from(&error, before).expect("deadline");
    // Fixed at receipt: receipt + 3600 s.
    assert!(
        delay >= Duration::from_secs(3600) && delay <= Duration::from_secs(3605),
        "{delay:?}"
    );
    assert_eq!(
        rss_ai_news_domain::error::ClassifiedError::error_kind(&error),
        "http_429"
    );
    assert!(rss_ai_news_domain::error::ClassifiedError::is_retryable(
        &error
    ));
}

#[tokio::test]
async fn retry_after_http_date_on_429_is_kept_as_that_instant() {
    let at = std::time::SystemTime::now() + Duration::from_secs(1800);
    let date = httpdate::fmt_http_date(at);
    let error = status_429(Some(&date)).await;
    // The date itself (whole-second precision), not a delay re-anchored later.
    assert_eq!(
        error.retry_at(),
        Some(httpdate::parse_http_date(&date).unwrap())
    );
}

/// R3E-S10: a 48 h wait (either form) is capped at receipt + 1 day, an
/// absolute deadline.
#[tokio::test]
async fn retry_after_of_48_hours_is_capped_one_day_after_receipt() {
    let day = Duration::from_secs(24 * 60 * 60);
    let in_48h = httpdate::fmt_http_date(std::time::SystemTime::now() + 2 * day);
    for value in ["172800", in_48h.as_str()] {
        let before = std::time::SystemTime::now();
        let error = status_429(Some(value)).await;
        let after = std::time::SystemTime::now();
        let at = error.retry_at().expect("deadline");
        assert!(at >= before + day && at <= after + day, "{value}: {at:?}");
    }
}

#[tokio::test]
async fn missing_or_invalid_retry_after_on_429_is_none() {
    assert_eq!(status_429(None).await.retry_at(), None);
    assert_eq!(status_429(Some("soon")).await.retry_at(), None);
    assert_eq!(status_429(Some("-5")).await.retry_at(), None);
    // A date in the past stays in the past: no extra wait beyond the backoff.
    assert_eq!(
        status_429(Some("Sun, 06 Nov 1994 08:49:37 GMT"))
            .await
            .retry_at(),
        Some(httpdate::parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap())
    );
}

/// R2-S11: reqwest sends the previous URL as `Referer` on redirects by
/// default; a signed article URL (`?sig=…`) must not leak to another host
/// through the production client.
#[tokio::test]
async fn cross_host_redirect_sends_no_referer() {
    let origin = MockServer::start().await;
    let other_host = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/article"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/moved", other_host.uri()).as_str()),
        )
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/moved"))
        .respond_with(ResponseTemplate::new(200).set_body_string(HTML_BODY))
        .mount(&other_host)
        .await;

    let fetcher = ReqwestHtmlFetcher::new(1024 * 1024).expect("fetcher should build");
    fetcher
        .fetch_html(&task(&origin, "/article?sig=secret-token"))
        .await
        .expect("redirected fetch succeeds");

    let received = other_host.received_requests().await.expect("recording");
    assert_eq!(received.len(), 1);
    assert!(
        received[0].headers.get("referer").is_none(),
        "no Referer on the redirected request: {:?}",
        received[0].headers.get("referer")
    );
    for (name, value) in &received[0].headers {
        assert!(
            !value.to_str().unwrap_or_default().contains("secret-token"),
            "header {name} leaks the signed URL"
        );
    }
}
