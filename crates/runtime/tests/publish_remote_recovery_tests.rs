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
use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_publish::{
    GitHubTarget, GitHubTargetConfig, LocalFsTarget, PublishError, PublishTarget,
    PublishedArtifact, PublishedBatchArtifact,
};
use rss_ai_news_runtime::{
    PublishFlow, PublishRemoteBatchItemOptions, PublishRemoteBatchOptions, PublishRemoteOptions,
    PublishRemoteStatus,
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
        let inner = GitHubTarget::with_base_uri(
            GitHubTargetConfig {
                token: rss_ai_news_domain::SecretString::from("recovery-test-token"),
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                branch: "main".to_string(),
                path_prefix: "reports".to_string(),
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
