//! R5-9: a [`WriteGuard`] is asked right before each request that changes
//! the branch — after the reads the request is conditional on — and a refusal
//! stops the call before that request: no contents PUT, no ref PATCH. A 422
//! non-fast-forward rebuild asks again before its own PATCH.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_publish::{
    GitHubTarget, GitHubTargetConfig, PublishError, PublishTarget, WriteGuard,
};
use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

fn report(category: &str, id: i64) -> RenderedReport {
    RenderedReport {
        publish_record_id: id,
        category_key: category.to_string(),
        report_date: "2026-04-29".to_string(),
        markdown_content: format!("# {category}\n"),
        relative_path: format!("{category}/2026-04-29.md"),
    }
}

/// Answers from a script, one answer per `confirm`, and records how many
/// requests the server had seen at each call (what preceded the check).
struct ScriptedGuard {
    answers: Mutex<Vec<bool>>,
    calls: AtomicUsize,
}

impl ScriptedGuard {
    fn new(answers: &[bool]) -> Self {
        Self {
            answers: Mutex::new(answers.iter().rev().copied().collect()),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl WriteGuard for ScriptedGuard {
    async fn confirm(&self) -> Result<(), PublishError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.answers.lock().unwrap().pop().unwrap_or(false) {
            Ok(())
        } else {
            Err(PublishError::WriteNotConfirmed {
                reason: "taken over".to_string(),
            })
        }
    }
}

async fn requests(server: &MockServer, verb: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == verb)
        .map(|request| request.url.path().to_string())
        .collect()
}

async fn mount_absent_contents(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(server)
        .await;
}

/// Single report: the guard is asked after the contents read, and a refusal
/// sends no PUT.
#[tokio::test]
async fn refused_guard_sends_no_contents_put() {
    let server = MockServer::start().await;
    mount_absent_contents(&server).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "content": { "sha": "file-sha" },
            "commit": { "sha": "commit" }
        })))
        .mount(&server)
        .await;
    let guard = ScriptedGuard::new(&[false]);

    let result = target(&server)
        .publish_guarded(&report("ai", 1), &guard)
        .await;

    assert!(
        matches!(result, Err(PublishError::WriteNotConfirmed { .. })),
        "{result:?}"
    );
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        requests(&server, "GET").await.len(),
        1,
        "the read the PUT depends on came first"
    );
    assert!(requests(&server, "PUT").await.is_empty());
}

/// Single report, guard confirmed: the PUT goes out.
#[tokio::test]
async fn confirmed_guard_lets_the_contents_put_through() {
    let server = MockServer::start().await;
    mount_absent_contents(&server).await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "content": { "sha": "file-sha" },
            "commit": { "sha": "commit" }
        })))
        .mount(&server)
        .await;
    let guard = ScriptedGuard::new(&[true]);

    let artifact = target(&server)
        .publish_guarded(&report("ai", 1), &guard)
        .await
        .unwrap();

    assert_eq!(artifact.commit_sha.as_deref(), Some("commit"));
    assert_eq!(requests(&server, "PUT").await.len(), 1);
}

async fn mount_batch_objects(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": { "sha": "head" }
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/commits/head"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tree": { "sha": "tree" }
        })))
        .mount(server)
        .await;
    mount_absent_contents(server).await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/git/trees"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "sha": "new-tree" })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/git/commits"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "sha": "new-commit" })))
        .mount(server)
        .await;
}

/// Batch: the guard is asked after HEAD was read and the commit built on
/// it, and a refusal sends no ref PATCH (the tree and commit objects are
/// unreachable from the branch).
#[tokio::test]
async fn refused_guard_sends_no_ref_patch() {
    let server = MockServer::start().await;
    mount_batch_objects(&server).await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": { "sha": "new-commit" }
        })))
        .mount(&server)
        .await;
    let guard = ScriptedGuard::new(&[false]);

    let result = target(&server)
        .publish_many_guarded(&[report("ai", 1), report("tech", 2)], &guard)
        .await;

    assert!(
        matches!(result, Err(PublishError::WriteNotConfirmed { .. })),
        "{result:?}"
    );
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        requests(&server, "POST").await.len(),
        2,
        "tree and commit built first"
    );
    assert!(requests(&server, "PATCH").await.is_empty());
}

/// Batch with a 422 non-fast-forward: the rebuild from the fresh HEAD asks
/// the guard again before its own PATCH; refused there, no second PATCH.
#[tokio::test]
async fn rebuild_after_a_non_fast_forward_asks_the_guard_again() {
    let server = MockServer::start().await;
    mount_batch_objects(&server).await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Update is not a fast forward"
        })))
        .mount(&server)
        .await;
    let guard = ScriptedGuard::new(&[true, false]);

    let result = target(&server)
        .publish_many_guarded(&[report("ai", 1), report("tech", 2)], &guard)
        .await;

    assert!(
        matches!(result, Err(PublishError::WriteNotConfirmed { .. })),
        "{result:?}"
    );
    assert_eq!(guard.calls.load(Ordering::SeqCst), 2);
    assert_eq!(requests(&server, "PATCH").await.len(), 1);
}
