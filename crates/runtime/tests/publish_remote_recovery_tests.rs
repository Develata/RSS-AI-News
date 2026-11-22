//! R4-1: a record released as `remote_write_unverified` enters recovery on its
//! next claim. Before any write, a read-only fixed-HEAD content check decides:
//! the content is there → the receipt is recorded without a write; it is
//! confirmed absent → the normal write, charged normally; the check fails
//! again → the attempt is deferred (refunded, state and snapshot ownership
//! kept). Until that check gives an answer, no retryable failure of the
//! attempt may charge the stage budget or drop the recovery mark.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::publish::{RemoteBlob, RenderedReport};
use rss_ai_news_publish::{
    GitHubTarget, GitHubTargetConfig, LocalFsTarget, PublishError, PublishTarget,
    PublishedArtifact, PublishedBatchArtifact,
};
use rss_ai_news_runtime::{
    PublishFlow, PublishRemoteBatchItemOptions, PublishRemoteBatchOptions, PublishRemoteOptions,
    PublishRemoteStatus,
};
use rss_ai_news_storage::{
    ClaimRequest, ClaimedPublishRecord, NewPublishRecord, PublishAdvanceExtras, PublishRecord,
    PublishRecordRepository, PublishState, PublishTimestampField, ReleaseFailureOutcome,
    StorageError, TerminalAdvanceOutcome, UnverifiedWriteUpdate,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{app_config, make_test_pool, publish_deps, seed_rendered_publish_record};

const UNVERIFIED: &str = "remote_write_unverified";

fn fixed_time() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(0).unwrap()
}

async fn seed_stored_local(pool: &SqlitePool) -> i64 {
    let rendered_at = fixed_time();
    let id = seed_rendered_publish_record(
        pool,
        Some("github://owner/repo/main/reports/ai.md".to_string()),
        rendered_at,
    )
    .await;
    sqlx::query(
        "UPDATE publish_records SET state = 'stored_local', local_stored_at = ?, \
         local_path = 'local/report.md', remote_target = 'owner/repo:main', \
         lease_owner = NULL, lease_expires_at = NULL WHERE id = ?",
    )
    .bind(rendered_at)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// The state an earlier attempt's `release_deferred_retry` leaves behind.
async fn mark_unverified(pool: &SqlitePool, id: i64) {
    sqlx::query(
        "UPDATE publish_records SET last_error = 'github write outcome unknown', \
         last_error_kind = ? WHERE id = ?",
    )
    .bind(UNVERIFIED)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// `(state, attempt_count, stage_attempt_count, last_error_kind, lease_owner)`.
async fn record_row(
    pool: &SqlitePool,
    id: i64,
) -> (String, i64, i64, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT state, attempt_count, stage_attempt_count, last_error_kind, lease_owner \
         FROM publish_records WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn article_states(pool: &SqlitePool, id: i64) -> Vec<String> {
    let states: Vec<String> = sqlx::query_scalar(
        "SELECT a.state FROM articles a JOIN publish_items pi ON pi.article_id = a.id \
         WHERE pi.publish_record_id = ? ORDER BY a.id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(!states.is_empty());
    states
}

async fn assert_kept_and_refunded(pool: &SqlitePool, id: i64) {
    assert_eq!(
        record_row(pool, id).await,
        (
            "stored_local".to_string(),
            0,
            0,
            Some(UNVERIFIED.to_string()),
            None
        ),
        "state kept, claim refunded, recovery mark kept, lease cleared"
    );
    assert!(
        article_states(pool, id)
            .await
            .iter()
            .all(|state| state == "ready_for_publish"),
        "the snapshot keeps its articles"
    );
}

fn remote_opts() -> PublishRemoteOptions {
    PublishRemoteOptions {
        category_display_name: "AI".to_string(),
        report_title: "Daily AI".to_string(),
        generated_at: fixed_time(),
        path_template: None,
    }
}

fn batch_item(publish_record_id: i64, name: &str) -> PublishRemoteBatchItemOptions {
    PublishRemoteBatchItemOptions {
        publish_record_id,
        category_display_name: name.to_string(),
        report_title: format!("Daily {name}"),
        generated_at: fixed_time(),
        path_template: None,
    }
}

fn git_blob_sha(content: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", content.len()).as_bytes());
    hasher.update(content);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A real [`GitHubTarget`] that also remembers the last report it was given,
/// so a test can answer GitHub's blob lookup with the report's blob id.
struct Recording {
    inner: GitHubTarget,
    last_markdown: Mutex<Option<String>>,
}

impl Recording {
    fn new(uri: &str) -> Arc<Self> {
        Self::with_prefix(uri, "reports")
    }

    /// Same destination (`owner/repo:main`) under another `path_prefix`.
    fn with_prefix(uri: &str, path_prefix: &str) -> Arc<Self> {
        let inner = GitHubTarget::with_base_uri(
            GitHubTargetConfig {
                token: rss_ai_news_domain::SecretString::from("recovery-test-token"),
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                branch: "main".to_string(),
                path_prefix: path_prefix.to_string(),
                commit_message_prefix: "auto: publish".to_string(),
            },
            uri,
        )
        .unwrap();
        Arc::new(Self {
            inner,
            last_markdown: Mutex::new(None),
        })
    }

    fn remember(&self, report: &RenderedReport) {
        *self.last_markdown.lock().unwrap() = Some(report.markdown_content.clone());
    }

    fn last_blob_sha(&self) -> String {
        let markdown = self
            .last_markdown
            .lock()
            .unwrap()
            .clone()
            .expect("a report");
        git_blob_sha(markdown.as_bytes())
    }
}

#[async_trait]
impl PublishTarget for Recording {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        self.remember(report);
        self.inner.publish(report).await
    }

    fn target_identity(&self) -> Option<String> {
        self.inner.target_identity()
    }

    async fn find_published(
        &self,
        reports: &[RenderedReport],
    ) -> Result<Option<PublishedBatchArtifact>, PublishError> {
        if let Some(report) = reports.first() {
            self.remember(report);
        }
        self.inner.find_published(reports).await
    }

    fn remote_blob(&self, report: &RenderedReport) -> Option<RemoteBlob> {
        self.remember(report);
        self.inner.remote_blob(report)
    }

    async fn find_remote_blob(
        &self,
        blob: &RemoteBlob,
    ) -> Result<Option<PublishedArtifact>, PublishError> {
        self.inner.find_remote_blob(blob).await
    }
}

fn github_flow(pool: SqlitePool, target: Arc<Recording>) -> PublishFlow {
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.retry.publish_max_attempts = 1;
    PublishFlow::new(Arc::new(publish_deps(
        pool,
        Arc::new(app),
        Arc::new(LocalFsTarget::new(std::env::temp_dir())),
        Some(target),
    )))
}

async fn non_get_requests(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.method.as_str() != "GET")
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect()
}

/// Run 1 of the reviewer's sequence on the last budget: the PUT outcome is
/// unknown (502) and the HEAD check fails (GET ref 503), leaving
/// `(stored_local, 0, remote_write_unverified)`.
async fn first_attempt_left_unverified(server: &MockServer, flow: &PublishFlow, id: i64) {
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"message": "Not Found"})),
        )
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(502).set_body_json(serde_json::json!({"message": "Bad gateway"})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/ref/heads/main"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"message": "unavailable"})),
        )
        .mount(server)
        .await;
    let outcome = flow.publish_remote_record(id, remote_opts()).await;
    assert!(
        matches!(&outcome.status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        outcome.status
    );
}

/// The reviewer's sequence (R4-1): `(stored_local, 0, remote_write_unverified)`
/// → the next run's first GET answers 503 → the record must stay
/// `stored_local` with its articles and the claim refunded, sending no write;
/// once HEAD holds the report, the receipt is recorded without a write.
#[tokio::test]
async fn unverified_record_whose_recovery_check_fails_again_keeps_ownership_then_records_the_receipt()
 {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local(&pool).await;
    let server = MockServer::start().await;
    let target = Recording::new(&server.uri());
    let flow = github_flow(pool.clone(), target.clone());
    first_attempt_left_unverified(&server, &flow, id).await;
    assert_kept_and_refunded(&pool, id).await;

    // Next run: every read fails.
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"message": "unavailable"})),
        )
        .mount(&server)
        .await;
    let second = flow.publish_remote_record(id, remote_opts()).await;

    assert!(
        matches!(&second.status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        second.status
    );
    assert_kept_and_refunded(&pool, id).await;
    assert_eq!(
        non_get_requests(&server).await,
        Vec::<String>::new(),
        "no write before the check"
    );
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT context_json FROM run_events WHERE event_kind = 'publish_failed' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let last: serde_json::Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["error_kind"], UNVERIFIED);
    assert_eq!(last["attempt_refunded"], true);
    // The failed check's request, not an invented write status.
    assert_eq!(last["operation"], "GET ref");
    assert_eq!(last["http_status"], 503);

    // HEAD now holds the report: receipt at that commit, no write.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": { "sha": "landed-commit" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/"))
        .and(query_param("ref", "landed-commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "file",
            "sha": target.last_blob_sha(),
        })))
        .mount(&server)
        .await;
    let third = flow.publish_remote_record(id, remote_opts()).await;

    assert_eq!(third.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(third.commit_sha.as_deref(), Some("landed-commit"));
    assert_eq!(non_get_requests(&server).await, Vec::<String>::new());
    let (state, _, stage, kind, _) = record_row(&pool, id).await;
    assert_eq!((state.as_str(), stage, kind), ("published_remote", 0, None));
    assert!(
        article_states(&pool, id)
            .await
            .iter()
            .all(|state| state == "published")
    );
}

/// The recovery check is itself rate limited: nothing is written, the claim
/// is refunded, the recovery mark kept, and GitHub's deadline is persisted
/// so the next run inside it does not claim (or call GitHub).
#[tokio::test]
async fn unverified_record_whose_recovery_check_is_rate_limited_waits_out_the_deadline() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local(&pool).await;
    let server = MockServer::start().await;
    let flow = github_flow(pool.clone(), Recording::new(&server.uri()));
    first_attempt_left_unverified(&server, &flow, id).await;

    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "600")
                .set_body_json(serde_json::json!({"message": "API rate limit exceeded"})),
        )
        .mount(&server)
        .await;
    let before = OffsetDateTime::now_utc();
    let outcome = flow.publish_remote_record(id, remote_opts()).await;

    assert!(
        matches!(&outcome.status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        outcome.status
    );
    assert_kept_and_refunded(&pool, id).await;
    assert_eq!(non_get_requests(&server).await, Vec::<String>::new());
    let wait: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT next_attempt_at FROM publish_records WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let wait = wait.expect("the check's deadline is persisted");
    assert!(wait >= before + time::Duration::seconds(599), "{wait}");
    let requests_before = server.received_requests().await.unwrap_or_default().len();
    assert_eq!(
        flow.publish_remote_record(id, remote_opts()).await.status,
        PublishRemoteStatus::NothingToClaim
    );
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        requests_before,
        "no request inside the window"
    );
}

/// Recovery check confirms the report is absent at HEAD: the normal write
/// runs, and its failure is charged like any retryable failure (here the
/// last budget, so the record fails).
#[tokio::test]
async fn unverified_record_confirmed_absent_is_written_and_charged_normally() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local(&pool).await;
    let server = MockServer::start().await;
    let flow = github_flow(pool.clone(), Recording::new(&server.uri()));
    first_attempt_left_unverified(&server, &flow, id).await;

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": { "sha": "head-without-report" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"message": "Not Found"})),
        )
        .mount(&server)
        .await;
    // A refused write (409 is retryable and not ambiguous).
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(409).set_body_json(serde_json::json!({"message": "Conflict"})),
        )
        .mount(&server)
        .await;
    let outcome = flow.publish_remote_record(id, remote_opts()).await;

    assert!(
        matches!(&outcome.status, PublishRemoteStatus::Failed { error_kind } if error_kind == "github_api_error"),
        "{:?}",
        outcome.status
    );
    assert_eq!(non_get_requests(&server).await.len(), 1, "one normal PUT");
    let (state, _, stage, kind, _) = record_row(&pool, id).await;
    assert_eq!(
        (state.as_str(), stage, kind.as_deref()),
        ("failed", 1, Some("github_api_error")),
        "charged: confirmed absent means the normal budget applies"
    );
}

/// Confirmed absent, then the normal write succeeds.
#[tokio::test]
async fn unverified_record_confirmed_absent_publishes_with_a_normal_write() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local(&pool).await;
    let server = MockServer::start().await;
    let flow = github_flow(pool.clone(), Recording::new(&server.uri()));
    first_attempt_left_unverified(&server, &flow, id).await;

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": { "sha": "head-without-report" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"message": "Not Found"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "content": { "sha": "file-sha" },
            "commit": { "sha": "written-commit" }
        })))
        .mount(&server)
        .await;
    let outcome = flow.publish_remote_record(id, remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(outcome.commit_sha.as_deref(), Some("written-commit"));
    assert_eq!(non_get_requests(&server).await.len(), 1);
}

// === batch path ===

/// `find_published` answers per report path: `landed` paths are there,
/// `failing` paths make the lookup fail, everything else is absent.
/// `publish_many` records how many reports it was asked to write.
struct ScriptedTarget {
    landed: Vec<&'static str>,
    failing: Vec<&'static str>,
    written: AtomicUsize,
    batches: AtomicUsize,
}

impl ScriptedTarget {
    fn artifact(report: &RenderedReport, commit: &str) -> PublishedArtifact {
        PublishedArtifact {
            local_path: None,
            commit_sha: Some(commit.to_string()),
            remote_target: Some(format!("github://owner/repo/main/{}", report.relative_path)),
        }
    }
}

#[async_trait]
impl PublishTarget for ScriptedTarget {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        self.written.fetch_add(1, Ordering::SeqCst);
        Ok(Self::artifact(report, "single-commit"))
    }

    /// Records no blobs of its own (`remote_blob` stays `None`), but answers
    /// for a recorded write the same way it answers `find_published`.
    async fn find_remote_blob(
        &self,
        blob: &RemoteBlob,
    ) -> Result<Option<PublishedArtifact>, PublishError> {
        let path = blob.path.to_lowercase();
        if self.failing.iter().any(|name| path.contains(name)) {
            return Err(PublishError::api(503, "unavailable"));
        }
        if self.landed.iter().any(|name| path.contains(name)) {
            return Ok(Some(PublishedArtifact {
                local_path: None,
                commit_sha: Some("landed-commit".to_string()),
                remote_target: Some(format!("github://owner/repo/main/{}", blob.path)),
            }));
        }
        Ok(None)
    }

    async fn find_published(
        &self,
        reports: &[RenderedReport],
    ) -> Result<Option<PublishedBatchArtifact>, PublishError> {
        let contains = |list: &[&str], report: &RenderedReport| {
            list.iter()
                .any(|name| report.relative_path.to_lowercase().contains(name))
        };
        if reports.iter().any(|report| contains(&self.failing, report)) {
            return Err(PublishError::api(503, "unavailable"));
        }
        if reports.iter().all(|report| contains(&self.landed, report)) {
            return Ok(Some(PublishedBatchArtifact {
                commit_sha: Some("landed-commit".to_string()),
                artifacts: reports
                    .iter()
                    .map(|report| Self::artifact(report, "landed-commit"))
                    .collect(),
            }));
        }
        Ok(None)
    }

    async fn publish_many(
        &self,
        reports: &[RenderedReport],
    ) -> Result<PublishedBatchArtifact, PublishError> {
        self.batches.fetch_add(1, Ordering::SeqCst);
        self.written.fetch_add(reports.len(), Ordering::SeqCst);
        Ok(PublishedBatchArtifact {
            commit_sha: Some("batch-commit".to_string()),
            artifacts: reports
                .iter()
                .map(|report| Self::artifact(report, "batch-commit"))
                .collect(),
        })
    }
}

async fn set_category(pool: &SqlitePool, id: i64, category: &str) {
    sqlx::query("UPDATE publish_records SET category_key = ? WHERE id = ?")
        .bind(category)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

fn scripted_flow(pool: SqlitePool, target: Arc<ScriptedTarget>) -> PublishFlow {
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.retry.publish_max_attempts = 1;
    PublishFlow::new(Arc::new(publish_deps(
        pool,
        Arc::new(app),
        Arc::new(LocalFsTarget::new(std::env::temp_dir())),
        Some(target),
    )))
}

/// In a batch every recovering record is checked on its own: a landed one is
/// recorded from its receipt, one whose check fails is deferred, and only
/// the records not recovering (or confirmed absent) are written.
#[tokio::test]
async fn batch_checks_each_unverified_record_on_its_own_before_writing() {
    let (_dir, pool) = make_test_pool().await;
    let landed = seed_stored_local(&pool).await;
    let failing = seed_stored_local(&pool).await;
    let absent = seed_stored_local(&pool).await;
    let fresh = seed_stored_local(&pool).await;
    for (id, category) in [
        (landed, "landedcat"),
        (failing, "failingcat"),
        (absent, "absentcat"),
        (fresh, "freshcat"),
    ] {
        set_category(&pool, id, category).await;
    }
    for id in [landed, failing, absent] {
        mark_unverified(&pool, id).await;
    }
    let target = Arc::new(ScriptedTarget {
        landed: vec!["landedcat"],
        failing: vec!["failingcat"],
        written: AtomicUsize::new(0),
        batches: AtomicUsize::new(0),
    });
    let flow = scripted_flow(pool.clone(), target.clone());

    let outcome = flow
        .publish_remote_batch(PublishRemoteBatchOptions {
            items: vec![
                batch_item(landed, "Landed"),
                batch_item(failing, "Failing"),
                batch_item(absent, "Absent"),
                batch_item(fresh, "Fresh"),
            ],
        })
        .await;

    let status = |id: i64| {
        outcome
            .items
            .iter()
            .find(|item| item.publish_record_id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(status(landed).status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(status(landed).commit_sha.as_deref(), Some("landed-commit"));
    assert!(
        matches!(&status(failing).status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        status(failing).status
    );
    assert_kept_and_refunded(&pool, failing).await;
    for id in [absent, fresh] {
        assert_eq!(status(id).status, PublishRemoteStatus::PublishedRemote);
        assert_eq!(status(id).commit_sha.as_deref(), Some("batch-commit"));
    }
    assert_eq!(target.batches.load(Ordering::SeqCst), 1);
    assert_eq!(
        target.written.load(Ordering::SeqCst),
        2,
        "only the absent and the fresh record are written"
    );
}

// === a retryable failure before the check also defers ===

struct StalledPublishStart;

#[async_trait]
impl rss_ai_news_storage::RunEventRepository for StalledPublishStart {
    async fn insert(
        &self,
        event: &rss_ai_news_storage::NewRunEvent,
    ) -> Result<i64, rss_ai_news_storage::StorageError> {
        if event.event_kind == "publish_started" {
            std::future::pending().await
        } else {
            Ok(1)
        }
    }
}

/// The preparation of a recovering record runs out of time before the check
/// could run: on the last budget this must not fold the record to `failed`
/// (nor drop its recovery mark), because its earlier write may have landed.
#[tokio::test]
async fn unverified_record_whose_preparation_times_out_is_deferred() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local(&pool).await;
    mark_unverified(&pool, id).await;
    let target = Arc::new(ScriptedTarget {
        landed: vec![],
        failing: vec![],
        written: AtomicUsize::new(0),
        batches: AtomicUsize::new(0),
    });
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.lease.publish_duration_seconds = 1;
    app.retry.publish_max_attempts = 1;
    let mut deps = publish_deps(
        pool.clone(),
        Arc::new(app),
        Arc::new(LocalFsTarget::new(std::env::temp_dir())),
        Some(target.clone()),
    );
    deps.event_repo = Arc::new(StalledPublishStart);
    let flow = PublishFlow::new(Arc::new(deps));

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        flow.publish_remote_record(id, remote_opts()),
    )
    .await
    .expect("bounded by the lease");

    assert!(
        matches!(&outcome.status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        outcome.status
    );
    assert_kept_and_refunded(&pool, id).await;
    assert_eq!(target.written.load(Ordering::SeqCst), 0);
}

// === R5-1: the destination holds the report, its receipt is not recorded ===

/// The real repository, except that the first `failures` terminal advances
/// fail with `error()` before touching the database — the receipt of a
/// report the destination already holds is not recorded.
struct UnrecordedReceipts {
    inner: rss_ai_news_storage::PublishRecordRepo,
    failures: AtomicUsize,
    error: fn() -> StorageError,
}

impl UnrecordedReceipts {
    fn new(pool: &SqlitePool, failures: usize, error: fn() -> StorageError) -> Arc<Self> {
        Arc::new(Self {
            inner: rss_ai_news_storage::PublishRecordRepo::new(pool.clone()),
            failures: AtomicUsize::new(failures),
            error,
        })
    }
}

fn db_unavailable() -> StorageError {
    StorageError::Unavailable("injected: receipt write lost".to_string())
}

fn db_corruption() -> StorageError {
    StorageError::Corruption("injected: receipt write rejected".to_string())
}

#[async_trait]
impl PublishRecordRepository for UnrecordedReceipts {
    async fn create_if_new(&self, item: &NewPublishRecord) -> Result<Option<i64>, StorageError> {
        self.inner.create_if_new(item).await
    }
    async fn find_by_id(&self, id: i64) -> Result<Option<PublishRecord>, StorageError> {
        self.inner.find_by_id(id).await
    }
    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<PublishRecord>, StorageError> {
        self.inner.find_by_idempotency_key(key).await
    }
    async fn claim_pending_for_freeze(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.inner.claim_pending_for_freeze(request).await
    }
    async fn claim_pending_for_freeze_in_category(
        &self,
        request: &ClaimRequest,
        category_key: &str,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.inner
            .claim_pending_for_freeze_in_category(request, category_key)
            .await
    }
    async fn claim_frozen_for_render(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.inner.claim_frozen_for_render(request).await
    }
    async fn claim_rendered_for_local_store(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.inner.claim_rendered_for_local_store(request).await
    }
    async fn claim_local_for_remote_publish(
        &self,
        request: &ClaimRequest,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.inner.claim_local_for_remote_publish(request).await
    }
    async fn claim_publish_by_ids(
        &self,
        request: &ClaimRequest,
        state: PublishState,
        ids: &[i64],
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.inner.claim_publish_by_ids(request, state, ids).await
    }
    async fn claim_pending_for_freeze_by_id_in_category(
        &self,
        request: &ClaimRequest,
        id: i64,
        category_key: &str,
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.inner
            .claim_pending_for_freeze_by_id_in_category(request, id, category_key)
            .await
    }
    async fn claim_local_for_remote_publish_by_ids(
        &self,
        request: &ClaimRequest,
        ids: &[i64],
    ) -> Result<Vec<ClaimedPublishRecord>, StorageError> {
        self.inner
            .claim_local_for_remote_publish_by_ids(request, ids)
            .await
    }
    async fn release_advance(
        &self,
        id: i64,
        owner: &str,
        from: PublishState,
        to: PublishState,
        timestamp_field: PublishTimestampField,
        now: OffsetDateTime,
        extras: PublishAdvanceExtras,
    ) -> Result<bool, StorageError> {
        self.inner
            .release_advance(id, owner, from, to, timestamp_field, now, extras)
            .await
    }
    async fn release_retryable_failure_until(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        max_attempts: u32,
        retry_not_before: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> Result<ReleaseFailureOutcome, StorageError> {
        self.inner
            .release_retryable_failure_until(
                id,
                owner,
                error,
                kind,
                max_attempts,
                retry_not_before,
                now,
            )
            .await
    }
    async fn release_empty_snapshot(
        &self,
        id: i64,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        self.inner.release_empty_snapshot(id, owner, now).await
    }
    async fn release_freeze_conflict(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        self.inner
            .release_freeze_conflict(id, owner, error, kind, now)
            .await
    }
    async fn release_ownership_conflict(
        &self,
        id: i64,
        owner: &str,
        state: PublishState,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        self.inner
            .release_ownership_conflict(id, owner, state, error, kind, now)
            .await
    }
    async fn release_deferred_retry(
        &self,
        id: i64,
        owner: &str,
        state: PublishState,
        error: &str,
        kind: &str,
        retry_not_before: Option<OffsetDateTime>,
        unverified_write: UnverifiedWriteUpdate<'_>,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        self.inner
            .release_deferred_retry(
                id,
                owner,
                state,
                error,
                kind,
                retry_not_before,
                unverified_write,
                now,
            )
            .await
    }
    async fn release_permanent_failure(
        &self,
        id: i64,
        owner: &str,
        error: &str,
        kind: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        self.inner
            .release_permanent_failure(id, owner, error, kind, now)
            .await
    }
    async fn reclaim_expired_leases(&self, now: OffsetDateTime) -> Result<u64, StorageError> {
        self.inner.reclaim_expired_leases(now).await
    }
    async fn terminalize_exhausted(
        &self,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        self.inner.terminalize_exhausted(max_attempts, now).await
    }
    async fn release_terminal_advance_with_articles(
        &self,
        id: i64,
        owner: &str,
        from: PublishState,
        to: PublishState,
        timestamp_field: PublishTimestampField,
        promote_article_ids: Vec<i64>,
        extras: PublishAdvanceExtras,
        now: OffsetDateTime,
    ) -> Result<TerminalAdvanceOutcome, StorageError> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err((self.error)());
        }
        self.inner
            .release_terminal_advance_with_articles(
                id,
                owner,
                from,
                to,
                timestamp_field,
                promote_article_ids,
                extras,
                now,
            )
            .await
    }
}

fn flow_with_repo(
    pool: SqlitePool,
    target: Arc<dyn PublishTarget>,
    repo: Arc<UnrecordedReceipts>,
) -> PublishFlow {
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.retry.publish_max_attempts = 1;
    let mut deps = publish_deps(
        pool,
        Arc::new(app),
        Arc::new(LocalFsTarget::new(std::env::temp_dir())),
        Some(target),
    );
    deps.publish_record_repo = repo;
    PublishFlow::new(Arc::new(deps))
}

/// Anything still leased is past its lease, so the next run's maintenance
/// reclaims it and sweeps it if its stage budget is spent — the reviewer's
/// path to `failed`.
async fn expire_leases(pool: &SqlitePool) {
    sqlx::query(
        "UPDATE publish_records SET lease_expires_at = '2000-01-01T00:00:00.000Z' \
         WHERE lease_owner IS NOT NULL",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// HEAD (`landed-commit`) holds `blob_sha` at every contents path.
async fn mount_head_holding(server: &MockServer, blob_sha: String) {
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": { "sha": "landed-commit" }
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "file",
            "sha": blob_sha,
        })))
        .mount(server)
        .await;
}

async fn last_publish_failed(pool: &SqlitePool) -> serde_json::Value {
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT context_json FROM run_events WHERE event_kind = 'publish_failed' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    serde_json::from_str(events.last().expect("a publish_failed event")).unwrap()
}

async fn assert_published_without_a_write(
    server: &MockServer,
    pool: &SqlitePool,
    id: i64,
    outcome: &rss_ai_news_runtime::PublishRemoteOutcome,
) {
    assert_eq!(outcome.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(outcome.commit_sha.as_deref(), Some("landed-commit"));
    assert_eq!(non_get_requests(server).await, Vec::<String>::new());
    let (state, _, stage, kind, lease) = record_row(pool, id).await;
    assert_eq!(
        (state.as_str(), stage, kind, lease),
        ("published_remote", 0, None, None)
    );
    assert!(
        article_states(pool, id)
            .await
            .iter()
            .all(|state| state == "published")
    );
}

/// The reviewer's sequence (R5-1): a recovering record on its last budget
/// finds its report at HEAD, but recording the receipt hits a retryable
/// database error. It must not stay leased (to be reclaimed and swept to
/// `failed`, releasing articles GitHub already shows): it is deferred with
/// its recovery mark, and the next run records the receipt without a write.
#[tokio::test]
async fn recovered_receipt_not_recorded_is_deferred_and_recorded_next_run() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local(&pool).await;
    let server = MockServer::start().await;
    let target = Recording::new(&server.uri());
    let repo = UnrecordedReceipts::new(&pool, 1, db_unavailable);
    let flow = flow_with_repo(pool.clone(), target.clone(), repo.clone());
    first_attempt_left_unverified(&server, &flow, id).await;
    assert_kept_and_refunded(&pool, id).await;

    server.reset().await;
    mount_head_holding(&server, target.last_blob_sha()).await;
    let second = flow.publish_remote_record(id, remote_opts()).await;

    assert!(
        matches!(&second.status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        second.status
    );
    assert_eq!(repo.failures.load(Ordering::SeqCst), 0, "the advance ran");
    assert_kept_and_refunded(&pool, id).await;
    assert_eq!(non_get_requests(&server).await, Vec::<String>::new());
    let event = last_publish_failed(&pool).await;
    assert_eq!(event["error_kind"], UNVERIFIED);
    assert_eq!(event["cause_kind"], "db_unavailable");
    assert_eq!(event["attempt_refunded"], true);

    expire_leases(&pool).await;
    server.reset().await;
    mount_head_holding(&server, target.last_blob_sha()).await;
    let third = flow.publish_remote_record(id, remote_opts()).await;
    assert_published_without_a_write(&server, &pool, id, &third).await;
}

/// The same after a normal write that succeeded: GitHub holds the report,
/// the receipt write fails — retryable or not — and the record is deferred
/// for recovery instead of left leased; the next run records the receipt
/// from HEAD without writing again.
#[tokio::test]
async fn written_receipt_not_recorded_is_deferred_and_recorded_next_run() {
    type InjectedError = fn() -> StorageError;
    let cases: [(InjectedError, &str); 2] = [
        (db_unavailable, "db_unavailable"),
        (db_corruption, "corruption"),
    ];
    for (error, cause_kind) in cases {
        let (_dir, pool) = make_test_pool().await;
        let id = seed_stored_local(&pool).await;
        let server = MockServer::start().await;
        let target = Recording::new(&server.uri());
        let repo = UnrecordedReceipts::new(&pool, 1, error);
        let flow = flow_with_repo(pool.clone(), target.clone(), repo);
        Mock::given(method("GET"))
            .and(path_regex("^/repos/owner/repo/contents/"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"message": "Not Found"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "content": { "sha": "file-sha" },
                "commit": { "sha": "written-commit" }
            })))
            .mount(&server)
            .await;
        let first = flow.publish_remote_record(id, remote_opts()).await;

        assert!(
            matches!(&first.status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
            "{cause_kind}: {:?}",
            first.status
        );
        assert_kept_and_refunded(&pool, id).await;
        assert_eq!(last_publish_failed(&pool).await["cause_kind"], cause_kind);

        expire_leases(&pool).await;
        server.reset().await;
        mount_head_holding(&server, target.last_blob_sha()).await;
        let second = flow.publish_remote_record(id, remote_opts()).await;
        assert_published_without_a_write(&server, &pool, id, &second).await;
    }
}

/// In a batch only the record whose receipt was not recorded is deferred;
/// the others are published, and the next batch records the deferred one
/// from HEAD without writing it again.
#[tokio::test]
async fn batch_receipt_not_recorded_defers_only_that_record() {
    let (_dir, pool) = make_test_pool().await;
    let first = seed_stored_local(&pool).await;
    let second = seed_stored_local(&pool).await;
    set_category(&pool, first, "firstcat").await;
    set_category(&pool, second, "secondcat").await;
    let repo = UnrecordedReceipts::new(&pool, 1, db_unavailable);
    let writer = Arc::new(ScriptedTarget {
        landed: vec![],
        failing: vec![],
        written: AtomicUsize::new(0),
        batches: AtomicUsize::new(0),
    });
    let flow = flow_with_repo(pool.clone(), writer.clone(), repo.clone());
    let items = || PublishRemoteBatchOptions {
        items: vec![batch_item(first, "First"), batch_item(second, "Second")],
    };
    let status = |outcome: &rss_ai_news_runtime::PublishRemoteBatchOutcome, id: i64| {
        outcome
            .items
            .iter()
            .find(|item| item.publish_record_id == id)
            .unwrap()
            .status
            .clone()
    };

    let outcome = flow.publish_remote_batch(items()).await;

    assert!(
        matches!(&status(&outcome, first), PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        status(&outcome, first)
    );
    assert_kept_and_refunded(&pool, first).await;
    assert_eq!(
        status(&outcome, second),
        PublishRemoteStatus::PublishedRemote
    );
    assert_eq!(writer.written.load(Ordering::SeqCst), 2);

    expire_leases(&pool).await;
    let landed = Arc::new(ScriptedTarget {
        landed: vec!["firstcat"],
        failing: vec![],
        written: AtomicUsize::new(0),
        batches: AtomicUsize::new(0),
    });
    let flow = flow_with_repo(pool.clone(), landed.clone(), repo);
    let rerun = flow.publish_remote_batch(items()).await;

    assert_eq!(status(&rerun, first), PublishRemoteStatus::PublishedRemote);
    assert_eq!(landed.written.load(Ordering::SeqCst), 0, "no write again");
    let (state, _, _, kind, _) = record_row(&pool, first).await;
    assert_eq!((state.as_str(), kind), ("published_remote", None));
}

// === R5-2: recovery checks the recorded write, not the current paths ===

/// `(unverified_write_path, unverified_write_blob_sha)`.
async fn recorded_write(pool: &SqlitePool, id: i64) -> (Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT unverified_write_path, unverified_write_blob_sha FROM publish_records \
         WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// HEAD (`landed-commit`) holds `blob_sha` under `reports/` only; the same
/// report under `archive/` is absent.
async fn mount_head_holding_only_under_reports(server: &MockServer, blob_sha: String) {
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": { "sha": "landed-commit" }
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/reports/"))
        .and(query_param("ref", "landed-commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "type": "file",
            "sha": blob_sha,
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/archive/"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"message": "Not Found"})),
        )
        .mount(server)
        .await;
}

async fn contents_paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|request| request.url.path().to_string())
        .filter(|path| path.starts_with("/repos/owner/repo/contents/"))
        .collect()
}

/// The reviewer's sequence (R5-2): the write under `reports/` is ambiguous
/// and its check fails; `github_path_prefix` then changes to `archive`. The
/// recovery must look for the recorded blob under `reports/` — where the
/// write went — find it there and record that receipt, never confirm the
/// report "absent" under `archive/` and write it a second time.
#[tokio::test]
async fn recovery_after_a_prefix_change_checks_the_recorded_path_and_writes_nothing() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local(&pool).await;
    let server = MockServer::start().await;
    let before = Recording::new(&server.uri());
    first_attempt_left_unverified(&server, &github_flow(pool.clone(), before.clone()), id).await;
    assert_kept_and_refunded(&pool, id).await;
    let (path, blob_sha) = recorded_write(&pool, id).await;
    let path = path.expect("the ambiguous write's path is recorded");
    assert!(path.starts_with("reports/"), "{path}");
    assert_eq!(blob_sha.as_deref(), Some(before.last_blob_sha().as_str()));

    // The prefix changes; the next check fails again: the record keeps the
    // write it inherited.
    let after = Recording::with_prefix(&server.uri(), "archive");
    let flow = github_flow(pool.clone(), after.clone());
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"message": "unavailable"})),
        )
        .mount(&server)
        .await;
    let second = flow.publish_remote_record(id, remote_opts()).await;
    assert!(
        matches!(&second.status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        second.status
    );
    assert_kept_and_refunded(&pool, id).await;
    assert_eq!(
        recorded_write(&pool, id).await,
        (Some(path.clone()), blob_sha.clone()),
        "kept"
    );

    // HEAD holds the report under the old prefix only.
    server.reset().await;
    mount_head_holding_only_under_reports(&server, blob_sha.clone().unwrap()).await;
    let third = flow.publish_remote_record(id, remote_opts()).await;

    assert_eq!(third.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(third.commit_sha.as_deref(), Some("landed-commit"));
    assert_eq!(
        third.remote_target,
        Some(format!("github://owner/repo/main/{path}"))
    );
    assert_eq!(non_get_requests(&server).await, Vec::<String>::new());
    let looked_up = contents_paths(&server).await;
    assert!(
        !looked_up.is_empty() && looked_up.iter().all(|p| p.contains("/contents/reports/")),
        "only the recorded path is looked up: {looked_up:?}"
    );
    let (state, _, _, kind, _) = record_row(&pool, id).await;
    assert_eq!((state.as_str(), kind), ("published_remote", None));
    assert_eq!(recorded_write(&pool, id).await, (None, None), "cleared");
}

/// R5-1 meets R5-2: a write under `reports/` succeeds but its receipt is not
/// recorded; after the prefix changes to `archive`, the next run finds the
/// recorded blob under `reports/` and records it without writing.
#[tokio::test]
async fn unrecorded_receipt_is_found_at_its_recorded_path_after_a_prefix_change() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local(&pool).await;
    let server = MockServer::start().await;
    let before = Recording::new(&server.uri());
    let repo = UnrecordedReceipts::new(&pool, 1, db_unavailable);
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"message": "Not Found"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "content": { "sha": "file-sha" },
            "commit": { "sha": "written-commit" }
        })))
        .mount(&server)
        .await;
    let first = flow_with_repo(pool.clone(), before.clone(), repo.clone())
        .publish_remote_record(id, remote_opts())
        .await;
    assert!(
        matches!(&first.status, PublishRemoteStatus::Failed { error_kind } if error_kind == UNVERIFIED),
        "{:?}",
        first.status
    );
    assert_kept_and_refunded(&pool, id).await;
    let (path, blob_sha) = recorded_write(&pool, id).await;
    let path = path.expect("the receipt's path is recorded");
    assert!(path.starts_with("reports/"), "{path}");
    assert_eq!(blob_sha.as_deref(), Some(before.last_blob_sha().as_str()));

    let after = Recording::with_prefix(&server.uri(), "archive");
    server.reset().await;
    mount_head_holding_only_under_reports(&server, blob_sha.unwrap()).await;
    let second = flow_with_repo(pool.clone(), after, repo)
        .publish_remote_record(id, remote_opts())
        .await;

    assert_eq!(second.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(
        second.remote_target,
        Some(format!("github://owner/repo/main/{path}"))
    );
    assert_eq!(non_get_requests(&server).await, Vec::<String>::new());
}

/// In a batch each recovering record is checked against its own recorded
/// write: the landed one is recorded at its recorded path without a write,
/// the absent one is written with the batch.
#[tokio::test]
async fn batch_recovery_checks_each_recorded_write() {
    let (_dir, pool) = make_test_pool().await;
    let landed = seed_stored_local(&pool).await;
    let absent = seed_stored_local(&pool).await;
    set_category(&pool, landed, "landedcat").await;
    set_category(&pool, absent, "absentcat").await;
    for (id, recorded) in [
        (landed, "old-prefix/landedcat.md"),
        (absent, "old-prefix/absentcat.md"),
    ] {
        mark_unverified(&pool, id).await;
        sqlx::query(
            "UPDATE publish_records SET unverified_write_path = ?, \
             unverified_write_blob_sha = 'recorded-blob' WHERE id = ?",
        )
        .bind(recorded)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    // `find_published` would call neither landed (no name matches the
    // current paths): only the recorded paths can say "landed".
    let target = Arc::new(ScriptedTarget {
        landed: vec!["old-prefix/landedcat"],
        failing: vec![],
        written: AtomicUsize::new(0),
        batches: AtomicUsize::new(0),
    });
    let flow = scripted_flow(pool.clone(), target.clone());

    let outcome = flow
        .publish_remote_batch(PublishRemoteBatchOptions {
            items: vec![batch_item(landed, "Landed"), batch_item(absent, "Absent")],
        })
        .await;

    let item = |id: i64| {
        outcome
            .items
            .iter()
            .find(|item| item.publish_record_id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(item(landed).status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(item(landed).commit_sha.as_deref(), Some("landed-commit"));
    assert_eq!(
        item(landed).remote_target.as_deref(),
        Some("github://owner/repo/main/old-prefix/landedcat.md")
    );
    assert_eq!(item(absent).status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(item(absent).commit_sha.as_deref(), Some("batch-commit"));
    assert_eq!(
        target.written.load(Ordering::SeqCst),
        1,
        "only the absent one"
    );
}
