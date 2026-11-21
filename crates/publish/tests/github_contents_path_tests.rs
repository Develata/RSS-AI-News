//! B15 (incident 2026-11-21): single-report Contents API requests must carry
//! the repository path percent-encoded per segment (UTF-8, RFC 3986) and the
//! branch as an encoded `ref` query parameter, on both the GET (existing sha)
//! and the PUT. Failures keep the operation, status, sanitized message and the
//! `x-github-request-id`.
//!
//! The assertions read the request targets wiremock actually received.

use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_publish::{GitHubTarget, GitHubTargetConfig, PublishError, PublishTarget};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "contents-path-test-token";
/// Harmless stand-in for report text a server could echo back.
const BODY_MARKER: &str = "REPORT-BODY-MARKER-7f3a";

fn target(server: &MockServer, branch: &str) -> GitHubTarget {
    GitHubTarget::with_base_uri(
        GitHubTargetConfig {
            token: rss_ai_news_domain::SecretString::from(TOKEN),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: branch.to_string(),
            path_prefix: "archive".to_string(),
            commit_message_prefix: "auto: publish".to_string(),
        },
        &server.uri(),
    )
    .unwrap()
}

fn report(relative_path: &str) -> RenderedReport {
    RenderedReport {
        publish_record_id: 1,
        category_key: "tech".to_string(),
        report_date: "2026-11-21".to_string(),
        markdown_content: "# 科技新闻\n".to_string(),
        relative_path: relative_path.to_string(),
    }
}

async fn mount_put_ok(server: &MockServer) {
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "content": { "sha": "new-file-sha" },
            "commit": { "sha": "commit-sha" }
        })))
        .mount(server)
        .await;
}

async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("recording enabled")
}

fn only(requests: &[Request], verb: &str) -> Request {
    let matching: Vec<_> = requests
        .iter()
        .filter(|request| request.method.as_str() == verb)
        .cloned()
        .collect();
    assert_eq!(matching.len(), 1, "{verb} requests: {matching:?}");
    matching.into_iter().next().unwrap()
}

const CHINESE_PATH: &str =
    "/repos/owner/repo/contents/archive/%E7%A7%91%E6%8A%80%E6%96%B0%E9%97%BB/2026/20261121.md";

#[tokio::test]
async fn create_chinese_path_sends_encoded_get_then_put_without_sha() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    mount_put_ok(&server).await;

    let artifact = target(&server, "release/2026")
        .publish(&report("科技新闻/2026/20261121.md"))
        .await
        .expect("create succeeds");

    let requests = received(&server).await;
    let get = only(&requests, "GET");
    assert_eq!(get.url.path(), CHINESE_PATH);
    assert_eq!(get.url.query(), Some("ref=release%2F2026"));
    let put = only(&requests, "PUT");
    assert_eq!(put.url.path(), CHINESE_PATH);
    assert_eq!(put.url.query(), None);
    let body: Value = serde_json::from_slice(&put.body).unwrap();
    assert!(body.get("sha").is_none(), "create must not send a sha");
    assert_eq!(body["branch"], "release/2026");
    // The logical repository path (not the encoded one) is what is recorded.
    assert_eq!(
        artifact.remote_target.as_deref(),
        Some("github://owner/repo/release/2026/archive/科技新闻/2026/20261121.md")
    );
}

#[tokio::test]
async fn update_chinese_path_sends_encoded_get_then_put_with_existing_sha() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sha": "old-file-sha"})))
        .mount(&server)
        .await;
    mount_put_ok(&server).await;

    target(&server, "main")
        .publish(&report("科技新闻/2026/20261121.md"))
        .await
        .expect("update succeeds");

    let requests = received(&server).await;
    let get = only(&requests, "GET");
    assert_eq!(get.url.path(), CHINESE_PATH);
    assert_eq!(get.url.query(), Some("ref=main"));
    let put = only(&requests, "PUT");
    assert_eq!(put.url.path(), CHINESE_PATH);
    let body: Value = serde_json::from_slice(&put.body).unwrap();
    assert_eq!(body["sha"], "old-file-sha");
}

/// A space and the reserved `%`, `#`, `?` are encoded once inside their
/// segment; unencoded, `#`/`?` would cut the path into a fragment/query.
#[tokio::test]
async fn space_and_reserved_characters_are_encoded_exactly_once_per_segment() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    mount_put_ok(&server).await;

    target(&server, "main")
        .publish(&report("tech news/50%#1?.md"))
        .await
        .expect("create succeeds");

    let requests = received(&server).await;
    let expected = "/repos/owner/repo/contents/archive/tech%20news/50%25%231%3F.md";
    let get = only(&requests, "GET");
    assert_eq!(get.url.path(), expected);
    assert_eq!(get.url.query(), Some("ref=main"));
    assert_eq!(get.url.fragment(), None);
    assert_eq!(only(&requests, "PUT").url.path(), expected);
}

#[tokio::test]
async fn ascii_path_request_target_is_unchanged() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    mount_put_ok(&server).await;

    target(&server, "main")
        .publish(&report("TECH/2026/20261121.md"))
        .await
        .expect("create succeeds");

    let requests = received(&server).await;
    let expected = "/repos/owner/repo/contents/archive/TECH/2026/20261121.md";
    let get = only(&requests, "GET");
    assert_eq!(get.url.path(), expected);
    assert_eq!(get.url.query(), Some("ref=main"));
    assert_eq!(only(&requests, "PUT").url.path(), expected);
}

#[tokio::test]
async fn contents_get_failure_keeps_operation_status_message_and_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("x-github-request-id", "C0DE:1234:ABCD")
                // The server echoes request text: token, report marker, path.
                .set_body_json(json!({
                    "message": format!("Bad request for {TOKEN}: {BODY_MARKER} 科技新闻")
                })),
        )
        .mount(&server)
        .await;

    let error = target(&server, "main")
        .publish(&report("科技新闻/2026/20261121.md"))
        .await
        .unwrap_err();

    assert!(
        matches!(error, PublishError::GitHubApiError { status: 400, .. }),
        "{error:?}"
    );
    let shown = error.display_user();
    assert!(shown.contains("400"), "{shown}");
    assert!(shown.contains("GET contents"), "{shown}");
    // R1-P-B6: only the fixed-vocabulary reason survives, never server text.
    assert!(shown.contains("bad request"), "{shown}");
    assert!(!shown.contains("Bad request for"), "{shown}");
    assert!(
        shown.contains("x-github-request-id: C0DE:1234:ABCD"),
        "{shown}"
    );
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(
        !shown.contains(BODY_MARKER),
        "echoed text must not leak: {shown}"
    );
    assert!(!shown.contains("科技新闻"), "{shown}");
    assert!(
        received(&server)
            .await
            .iter()
            .all(|r| r.method.as_str() == "GET")
    );
}

#[tokio::test]
async fn contents_put_failure_keeps_operation_and_request_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(422)
                .insert_header("x-github-request-id", "B0D7:5678")
                .set_body_json(json!({"message": "Invalid request."})),
        )
        .mount(&server)
        .await;

    let error = target(&server, "main")
        .publish(&report("科技新闻/2026/20261121.md"))
        .await
        .unwrap_err();

    let shown = error.display_user();
    assert!(matches!(
        error,
        PublishError::GitHubApiError { status: 422, .. }
    ));
    assert!(shown.contains("PUT contents"), "{shown}");
    assert!(shown.contains("invalid request"), "{shown}");
    assert!(shown.contains("x-github-request-id: B0D7:5678"), "{shown}");
}

/// R1-P-B6: a request id that does not have GitHub's shape, or carries the
/// token, is dropped instead of being filtered into shape.
#[tokio::test]
async fn malformed_or_token_bearing_request_id_is_dropped() {
    for request_id in [
        TOKEN.to_string(),
        "not:an:id".to_string(),
        format!("ABCD:{BODY_MARKER}"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(400)
                    .insert_header("x-github-request-id", request_id.as_str())
                    .set_body_json(json!({"message": "Bad request"})),
            )
            .mount(&server)
            .await;

        let shown = target(&server, "main")
            .publish(&report("tech/2026/20261121.md"))
            .await
            .unwrap_err()
            .display_user();

        assert!(!shown.contains("x-github-request-id"), "{shown}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(!shown.contains(BODY_MARKER), "{shown}");
    }
}

/// The added request context must not change auth / rate-limit classification
/// (B5): a quota-exhausted 403 is still a retryable rate limit.
#[tokio::test]
async fn contents_rate_limited_403_is_still_retryable_with_request_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-github-request-id", "RATE:1")
                .insert_header("x-ratelimit-remaining", "0")
                .set_body_json(json!({"message": "Forbidden"})),
        )
        .mount(&server)
        .await;

    let error = target(&server, "main")
        .publish(&report("科技新闻/2026/20261121.md"))
        .await
        .unwrap_err();

    assert!(
        matches!(error, PublishError::GitHubRateLimit { .. }),
        "{error:?}"
    );
    assert!(error.is_retryable());
}

/// Raw request lines as they left the client. wiremock parses targets into a
/// `Url` (which normalizes non-ASCII), so this listener reads the bytes.
async fn raw_request_lines(
    responses: Vec<&'static str>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut lines = Vec::new();
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let head_text = String::from_utf8_lossy(&head).into_owned();
            lines.push(head_text.lines().next().unwrap_or_default().to_string());
            let length = head_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let mut body = vec![0_u8; length];
            socket.read_exact(&mut body).await.unwrap();
            let reply = format!(
                "HTTP/1.1 {response}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body_for(response).len(),
                body_for(response)
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        lines
    });
    (uri, handle)
}

fn body_for(status_line: &str) -> &'static str {
    if status_line.starts_with("404") {
        r#"{"message":"Not Found"}"#
    } else {
        r#"{"content":{"sha":"f"},"commit":{"sha":"c"}}"#
    }
}

#[tokio::test]
async fn chinese_path_leaves_the_client_percent_encoded_on_get_and_put() {
    let (uri, server) = raw_request_lines(vec!["404 Not Found", "201 Created"]).await;
    let target = GitHubTarget::with_base_uri(
        GitHubTargetConfig {
            token: rss_ai_news_domain::SecretString::from(TOKEN),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: "release/2026".to_string(),
            path_prefix: "archive".to_string(),
            commit_message_prefix: "auto: publish".to_string(),
        },
        &uri,
    )
    .unwrap();

    target
        .publish(&report("科技新闻/2026/20261121.md"))
        .await
        .expect("create succeeds");

    let lines = server.await.unwrap();
    assert_eq!(
        lines,
        vec![
            format!("GET {CHINESE_PATH}?ref=release%2F2026 HTTP/1.1"),
            format!("PUT {CHINESE_PATH} HTTP/1.1"),
        ]
    );
    assert!(lines.iter().all(|line| line.is_ascii()));
}
