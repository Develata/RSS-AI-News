use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_publish::{GitHubTarget, GitHubTargetConfig, PublishError, PublishTarget};
use serde_json::json;
use time::OffsetDateTime;
use wiremock::matchers::{any, body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CONTENT_BASE64: &str = "IyBoZWxsbwo=";
const GITHUB_PATH: &str = "/repos/owner/repo/contents/reports/tech/2026-04-29.md";

#[tokio::test]
async fn create_new_file_returns_commit_sha() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(GITHUB_PATH))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(GITHUB_PATH))
        .and(body_json(json!({
            "message": "auto: publish tech/2026-04-29.md",
            "content": CONTENT_BASE64,
            "branch": "main"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "content": { "sha": "file-sha" },
            "commit": { "sha": "deadbeef" }
        })))
        .mount(&server)
        .await;
    let target = target(&server);

    let artifact = target.publish(&sample_report()).await.unwrap();

    assert_eq!(artifact.commit_sha.as_deref(), Some("deadbeef"));
    assert_eq!(
        artifact.remote_target.as_deref(),
        Some("github://owner/repo/main/reports/tech/2026-04-29.md")
    );
    assert!(artifact.local_path.is_none());
}

#[tokio::test]
async fn update_existing_file_sends_existing_sha() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(GITHUB_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sha": "old-file-sha"})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(GITHUB_PATH))
        .and(body_json(json!({
            "message": "auto: publish tech/2026-04-29.md",
            "content": CONTENT_BASE64,
            "branch": "main",
            "sha": "old-file-sha"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": { "sha": "new-file-sha" },
            "commit": { "sha": "cafebabe" }
        })))
        .mount(&server)
        .await;
    let target = target(&server);

    let artifact = target.publish(&sample_report()).await.unwrap();

    assert_eq!(artifact.commit_sha.as_deref(), Some("cafebabe"));
}

#[tokio::test]
async fn auth_failure_maps_to_github_auth_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(GITHUB_PATH))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "message": "Bad credentials"
        })))
        .mount(&server)
        .await;
    let target = target(&server);

    let error = target.publish(&sample_report()).await.unwrap_err();

    assert!(matches!(
        error,
        PublishError::GitHubAuthFailed(reason) if reason == "Bad credentials"
    ));
}

#[tokio::test]
async fn rate_limit_maps_to_github_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(GITHUB_PATH))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("X-RateLimit-Reset", "1800")
                .set_body_json(json!({ "message": "rate limited" })),
        )
        .mount(&server)
        .await;
    let target = target(&server);

    let error = target.publish(&sample_report()).await.unwrap_err();

    assert!(matches!(
        error,
        PublishError::GitHubRateLimit { reset_at }
            if reset_at == OffsetDateTime::from_unix_timestamp(1_800).unwrap()
    ));
}

#[tokio::test]
async fn server_error_maps_to_retryable_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(GITHUB_PATH))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "message": "server unavailable"
        })))
        .mount(&server)
        .await;
    let target = target(&server);

    let error = target.publish(&sample_report()).await.unwrap_err();

    assert!(matches!(
        error,
        PublishError::GitHubApiError { status: 500, .. }
    ));
    assert!(error.is_retryable());
}

#[tokio::test]
async fn rejects_invalid_relative_path_without_http_request() {
    let server = MockServer::start().await;
    let _guard = Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount_as_scoped(&server)
        .await;
    let target = target(&server);
    let mut report = sample_report();
    report.relative_path = "tech/../escape.md".to_string();

    let error = target.publish(&report).await.unwrap_err();

    assert!(matches!(
        error,
        PublishError::InvalidPath(path) if path == "tech/../escape.md"
    ));
}

fn target(server: &MockServer) -> GitHubTarget {
    GitHubTarget::with_base_uri(
        GitHubTargetConfig {
            token: "token".to_string(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: "main".to_string(),
            path_prefix: "reports".to_string(),
            commit_message_prefix: "auto: publish".to_string(),
        },
        &server.uri(),
    )
    .unwrap()
}

fn sample_report() -> RenderedReport {
    RenderedReport {
        publish_record_id: 1,
        category_key: "tech".to_string(),
        report_date: "2026-04-29".to_string(),
        markdown_content: "# hello\n".to_string(),
        relative_path: "tech/2026-04-29.md".to_string(),
    }
}
