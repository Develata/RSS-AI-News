//! B9 (ADR 0012): multi-report publish is idempotent across a lost ref-update
//! response. Before creating a commit, the batch compares each path's blob at
//! the fetched HEAD commit with the git blob sha of the report; when every
//! path already matches, the receipts point at that HEAD commit and no new
//! commit is created.

use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_publish::{GitHubTarget, GitHubTargetConfig, PublishTarget};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// `printf '# hello\n' | git hash-object --stdin` and `printf '# ai\n' | …`
const HELLO_BLOB: &str = "8954bb97349bfe2a7799e6a7a64c6f747c635d6c";
const AI_BLOB: &str = "ac4465c28b0c1b4f8a63e6a696f955e5b8a696d5";

fn target(server: &MockServer) -> GitHubTarget {
    GitHubTarget::with_base_uri(
        GitHubTargetConfig {
            token: rss_ai_news_domain::SecretString::from("token"),
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

fn reports() -> Vec<RenderedReport> {
    [("tech", "# hello\n", 1), ("ai", "# ai\n", 2)]
        .into_iter()
        .map(|(category, content, id)| RenderedReport {
            publish_record_id: id,
            category_key: category.to_string(),
            report_date: "2026-04-29".to_string(),
            markdown_content: content.to_string(),
            relative_path: format!("{category}/2026-04-29.md"),
        })
        .collect()
}

async fn mount_head(server: &MockServer, commit: &str, tree: &str, times: Option<u64>) {
    let mut ref_mock = Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": { "sha": commit }
        })));
    if let Some(times) = times {
        ref_mock = ref_mock.up_to_n_times(times);
    }
    ref_mock.mount(server).await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/repo/git/commits/{commit}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": { "sha": tree }
        })))
        .mount(server)
        .await;
}

async fn mount_blob(server: &MockServer, file: &str, at: &str, sha: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/repo/contents/reports/{file}")))
        .and(query_param("ref", at))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "sha": sha })))
        .mount(server)
        .await;
}

async fn count(server: &MockServer, verb: &str, route: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == verb && request.url.path() == route)
        .count()
}

#[tokio::test]
async fn retry_after_lost_ref_update_response_records_existing_head_without_new_commit() {
    let server = MockServer::start().await;
    // Attempt 1: HEAD is the old commit, which lacks the reports.
    mount_head(&server, "old-head", "old-tree", Some(1)).await;
    // Attempt 2: the branch already points at the commit attempt 1 created.
    mount_head(&server, "batch-commit", "batch-tree", None).await;
    mount_blob(&server, "tech/2026-04-29.md", "batch-commit", HELLO_BLOB).await;
    mount_blob(&server, "ai/2026-04-29.md", "batch-commit", AI_BLOB).await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/git/trees"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "sha": "batch-tree" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/git/commits"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "sha": "batch-commit" })))
        .mount(&server)
        .await;
    // The ref update is applied, but its response is lost (gateway 502).
    Mock::given(method("PATCH"))
        .and(path("/repos/owner/repo/git/refs/heads/main"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({ "message": "Bad gateway" })))
        .mount(&server)
        .await;
    let target = target(&server);

    // P-B2: attempt 1 does not take the lost response as a failure; a
    // read-only HEAD check finds its own commit and records it.
    let first = target
        .publish_many(&reports())
        .await
        .expect("the ambiguous ref update is verified against HEAD");
    assert_eq!(first.commit_sha.as_deref(), Some("batch-commit"));
    // The HTTP client may itself resend the PATCH on a 5xx; all of that
    // belongs to attempt 1.
    let ref_updates_in_attempt_1 =
        count(&server, "PATCH", "/repos/owner/repo/git/refs/heads/main").await;
    assert!(ref_updates_in_attempt_1 >= 1);
    // A later run (e.g. the receipt was never persisted) is idempotent too.
    let second = target
        .publish_many(&reports())
        .await
        .expect("retry finds the batch already on HEAD");

    assert_eq!(second.commit_sha.as_deref(), Some("batch-commit"));
    assert_eq!(second.artifacts.len(), 2);
    assert!(
        second
            .artifacts
            .iter()
            .all(|artifact| artifact.commit_sha.as_deref() == Some("batch-commit"))
    );
    assert_eq!(
        second
            .artifacts
            .iter()
            .map(|artifact| artifact.remote_target.clone().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "github://owner/repo/main/reports/tech/2026-04-29.md".to_string(),
            "github://owner/repo/main/reports/ai/2026-04-29.md".to_string(),
        ],
        "one receipt per report, in input order"
    );
    assert_eq!(
        count(&server, "POST", "/repos/owner/repo/git/commits").await,
        1
    );
    assert_eq!(
        count(&server, "POST", "/repos/owner/repo/git/trees").await,
        1
    );
    assert_eq!(
        count(&server, "PATCH", "/repos/owner/repo/git/refs/heads/main").await,
        ref_updates_in_attempt_1,
        "the retry must not update the ref again"
    );
}

/// Only a positive match on every path counts: one differing blob means the
/// batch is not on HEAD, so a commit is created as before.
#[tokio::test]
async fn one_differing_blob_still_creates_a_commit() {
    let server = MockServer::start().await;
    mount_head(&server, "head", "tree", None).await;
    mount_blob(&server, "tech/2026-04-29.md", "head", HELLO_BLOB).await;
    mount_blob(
        &server,
        "ai/2026-04-29.md",
        "head",
        "0000000000000000000000000000000000000000",
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/git/trees"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "sha": "new-tree" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/git/commits"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "sha": "new-commit" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/owner/repo/git/refs/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": { "sha": "new-commit" }
        })))
        .mount(&server)
        .await;

    let batch = target(&server).publish_many(&reports()).await.unwrap();

    assert_eq!(batch.commit_sha.as_deref(), Some("new-commit"));
    assert_eq!(
        count(&server, "POST", "/repos/owner/repo/git/commits").await,
        1
    );
}

/// P-B2: an ambiguous ref update (5xx) whose commit is not on HEAD keeps its
/// original, retryable error; nothing is recorded as published.
#[tokio::test]
async fn ambiguous_ref_update_not_on_head_keeps_the_write_error() {
    let server = MockServer::start().await;
    mount_head(&server, "old-head", "old-tree", None).await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/git/trees"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "sha": "batch-tree" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/git/commits"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "sha": "batch-commit" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/owner/repo/git/refs/heads/main"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({ "message": "Bad gateway" })))
        .mount(&server)
        .await;

    let error = target(&server).publish_many(&reports()).await.unwrap_err();

    assert!(
        matches!(
            error,
            rss_ai_news_publish::PublishError::GitHubApiError { status: 502, .. }
        ),
        "{error:?}"
    );
    assert!(error.to_string().contains("PATCH ref"), "{error}");
}

fn single_report() -> RenderedReport {
    reports().remove(0)
}

/// P-B2, single report: a PUT whose response is a gateway 5xx is checked
/// read-only against the branch HEAD; the report being there is a success.
#[tokio::test]
async fn ambiguous_contents_put_is_verified_against_head() {
    let server = MockServer::start().await;
    mount_head(&server, "put-commit", "put-tree", None).await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/owner/repo/contents/reports/tech/2026-04-29.md",
        ))
        .and(query_param("ref", "main"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&server)
        .await;
    mount_blob(&server, "tech/2026-04-29.md", "put-commit", HELLO_BLOB).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(504).set_body_json(json!({ "message": "timeout" })))
        .mount(&server)
        .await;

    let artifact = target(&server)
        .publish(&single_report())
        .await
        .expect("the report is on HEAD");

    assert_eq!(artifact.commit_sha.as_deref(), Some("put-commit"));
    assert_eq!(
        artifact.remote_target.as_deref(),
        Some("github://owner/repo/main/reports/tech/2026-04-29.md")
    );
}

#[tokio::test]
async fn ambiguous_contents_put_not_on_head_keeps_the_write_error() {
    let server = MockServer::start().await;
    mount_head(&server, "other-commit", "other-tree", None).await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/owner/repo/contents/reports/tech/2026-04-29.md",
        ))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(504).set_body_json(json!({ "message": "timeout" })))
        .mount(&server)
        .await;

    let error = target(&server).publish(&single_report()).await.unwrap_err();

    assert!(
        matches!(
            error,
            rss_ai_news_publish::PublishError::GitHubApiError { status: 504, .. }
        ),
        "{error:?}"
    );
}

/// A refused write (4xx) is never checked: it cannot have landed.
#[tokio::test]
async fn refused_contents_put_is_not_verified() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/owner/repo/contents/reports/tech/2026-04-29.md",
        ))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({ "message": "conflict" })))
        .mount(&server)
        .await;

    target(&server).publish(&single_report()).await.unwrap_err();

    assert_eq!(
        count(&server, "GET", "/repos/owner/repo/git/ref/heads/main").await,
        0
    );
}

/// A lookup error is never read as "already published": the batch fails.
#[tokio::test]
async fn blob_lookup_error_is_not_treated_as_already_published() {
    let server = MockServer::start().await;
    mount_head(&server, "head", "tree", None).await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/owner/repo/contents/reports/tech/2026-04-29.md",
        ))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "boom" })))
        .mount(&server)
        .await;

    let error = target(&server).publish_many(&reports()).await.unwrap_err();

    assert!(error.to_string().contains("GET contents"), "{error}");
    assert_eq!(
        count(&server, "POST", "/repos/owner/repo/git/commits").await,
        0
    );
}
