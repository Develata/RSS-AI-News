mod common;

use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_publish::{
    LocalFsTarget, PublishError, PublishTarget, PublishedArtifact, PublishedBatchArtifact,
};
use rss_ai_news_runtime::{
    PublishFlow, PublishRemoteBatchItemOptions, PublishRemoteBatchOptions, PublishRemoteOptions,
    PublishRemoteStatus,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{app_config, make_test_pool, publish_deps, seed_rendered_publish_record};

#[tokio::test]
async fn publish_remote_succeeds_promotes_articles() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), Some(Arc::new(MockSuccessTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert_eq!(outcome.publish_record_id, record_id);
    assert_eq!(outcome.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(outcome.commit_sha.as_deref(), Some("remote-commit-sha"));
    assert_eq!(
        outcome.remote_target.as_deref(),
        Some("github://owner/repo/main/reports/ai.md")
    );
    assert_eq!(outcome.item_count, 1);
    assert_record_state(&pool, record_id, "published_remote").await;
    assert_record_remote_fields(&pool, record_id).await;
    assert_referenced_articles_state(&pool, record_id, "published").await;
}

#[tokio::test]
async fn publish_remote_record_claims_only_the_requested_record() {
    let (_dir, pool) = make_test_pool().await;
    let other_id = seed_stored_local_publish_record(&pool).await;
    let target_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), Some(Arc::new(MockSuccessTarget)));

    let outcome = flow.publish_remote_record(target_id, remote_opts()).await;

    assert_eq!(outcome.publish_record_id, target_id);
    assert_eq!(outcome.status, PublishRemoteStatus::PublishedRemote);
    assert_record_state(&pool, target_id, "published_remote").await;
    assert_record_state(&pool, other_id, "stored_local").await;
}

#[tokio::test]
async fn publish_remote_batch_succeeds_with_one_commit_for_multiple_records() {
    let (_dir, pool) = make_test_pool().await;
    let first_id = seed_stored_local_publish_record(&pool).await;
    let second_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), Some(Arc::new(MockBatchSuccessTarget)));

    let outcome = flow
        .publish_remote_batch(PublishRemoteBatchOptions {
            items: vec![
                batch_item(first_id, "AI", "Daily AI"),
                batch_item(second_id, "ML", "Daily ML"),
            ],
        })
        .await;

    assert_eq!(outcome.commit_sha.as_deref(), Some("batch-commit-sha"));
    assert_eq!(outcome.items.len(), 2);
    assert!(
        outcome
            .items
            .iter()
            .all(|item| item.status == PublishRemoteStatus::PublishedRemote)
    );
    assert!(
        outcome
            .items
            .iter()
            .all(|item| item.commit_sha.as_deref() == Some("batch-commit-sha"))
    );
    for record_id in [first_id, second_id] {
        assert_record_state(&pool, record_id, "published_remote").await;
        assert_referenced_articles_state(&pool, record_id, "published").await;
    }
}

#[tokio::test]
async fn publish_remote_rate_limit_keeps_state_and_articles() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), Some(Arc::new(MockRateLimitTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert!(matches!(
        outcome.status,
        PublishRemoteStatus::Failed { error_kind } if error_kind == "github_rate_limit"
    ));
    assert_record_state(&pool, record_id, "stored_local").await;
    assert_last_error_kind(&pool, record_id, "github_rate_limit").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
}

#[tokio::test]
async fn publish_remote_auth_failed_is_terminal_without_promoting_articles() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), Some(Arc::new(MockAuthFailTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert!(matches!(
        outcome.status,
        PublishRemoteStatus::Failed { error_kind } if error_kind == "github_auth_failed"
    ));
    assert_record_state(&pool, record_id, "failed").await;
    assert_last_error_kind(&pool, record_id, "github_auth_failed").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
}

#[tokio::test]
async fn publish_remote_missing_target_has_no_side_effects() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    let flow = flow(pool.clone(), None);

    let outcome = flow.publish_remote(remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::MissingTarget);
    assert_eq!(outcome.publish_record_id, 0);
    assert_record_state(&pool, record_id, "stored_local").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
    let attempt_count: i64 =
        sqlx::query_scalar("SELECT attempt_count FROM publish_records WHERE id = ?")
            .bind(record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempt_count, 0);
}

#[tokio::test]
async fn publish_remote_returns_nothing_to_claim_when_no_stored_local_record() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool, Some(Arc::new(MockSuccessTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::NothingToClaim);
    assert_eq!(outcome.publish_record_id, 0);
}

#[tokio::test]
async fn publish_remote_returns_nothing_to_claim_when_record_is_leased_by_other_owner() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_publish_record(&pool).await;
    sqlx::query(
        "UPDATE publish_records SET lease_owner = 'other-owner', lease_expires_at = ? WHERE id = ?",
    )
    .bind(OffsetDateTime::now_utc() + time::Duration::seconds(300))
    .bind(record_id)
    .execute(&pool)
    .await
    .unwrap();
    let flow = flow(pool.clone(), Some(Arc::new(MockSuccessTarget)));

    let outcome = flow.publish_remote(remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::NothingToClaim);
    assert_record_state(&pool, record_id, "stored_local").await;
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
}

async fn seed_stored_local_publish_record(pool: &SqlitePool) -> i64 {
    let rendered_at = fixed_time();
    let record_id = seed_rendered_publish_record(
        pool,
        Some("github://owner/repo/main/reports/ai.md".to_string()),
        rendered_at,
    )
    .await;
    sqlx::query(
        r#"
        UPDATE publish_records
        SET state = 'stored_local',
            local_stored_at = ?,
            local_path = 'local/report.md',
            lease_owner = NULL,
            lease_expires_at = NULL
        WHERE id = ?
        "#,
    )
    .bind(rendered_at)
    .bind(record_id)
    .execute(pool)
    .await
    .unwrap();
    record_id
}

fn flow(pool: SqlitePool, remote_target: Option<Arc<dyn PublishTarget>>) -> PublishFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let output_dir = tempfile::tempdir().unwrap();
    let ctx = Arc::new(publish_deps(
        pool,
        app,
        Arc::new(LocalFsTarget::new(output_dir.path().to_path_buf())),
        remote_target,
    ));
    PublishFlow::new(ctx)
}

async fn assert_record_state(pool: &SqlitePool, id: i64, expected: &str) {
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, expected);
}

async fn assert_record_remote_fields(pool: &SqlitePool, id: i64) {
    let row: (Option<String>, Option<String>, Option<OffsetDateTime>, Option<String>) =
        sqlx::query_as(
            "SELECT commit_sha, remote_target, remote_published_at, local_path FROM publish_records WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(row.0.as_deref(), Some("remote-commit-sha"));
    assert_eq!(
        row.1.as_deref(),
        Some("github://owner/repo/main/reports/ai.md")
    );
    assert!(row.2.is_some());
    assert_eq!(row.3.as_deref(), Some("local/report.md"));
}

async fn assert_last_error_kind(pool: &SqlitePool, id: i64, expected: &str) {
    let kind: Option<String> =
        sqlx::query_scalar("SELECT last_error_kind FROM publish_records WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(kind.as_deref(), Some(expected));
}

async fn assert_referenced_articles_state(pool: &SqlitePool, record_id: i64, expected: &str) {
    let states: Vec<String> = sqlx::query_scalar(
        "SELECT a.state FROM articles a JOIN publish_items pi ON pi.article_id = a.id WHERE pi.publish_record_id = ? ORDER BY a.id",
    )
    .bind(record_id)
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(!states.is_empty());
    assert!(states.iter().all(|state| state == expected));
}

fn remote_opts() -> PublishRemoteOptions {
    PublishRemoteOptions {
        category_display_name: "AI".to_string(),
        report_title: "Daily AI".to_string(),
        generated_at: fixed_time(),
        path_template: None,
    }
}

fn batch_item(
    publish_record_id: i64,
    category_display_name: &str,
    report_title: &str,
) -> PublishRemoteBatchItemOptions {
    PublishRemoteBatchItemOptions {
        publish_record_id,
        category_display_name: category_display_name.to_string(),
        report_title: report_title.to_string(),
        generated_at: fixed_time(),
        path_template: None,
    }
}

fn fixed_time() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(0).unwrap()
}

struct MockSuccessTarget;

#[async_trait]
impl PublishTarget for MockSuccessTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        Ok(PublishedArtifact {
            local_path: None,
            commit_sha: Some("remote-commit-sha".to_string()),
            remote_target: Some("github://owner/repo/main/reports/ai.md".to_string()),
        })
    }
}

struct MockBatchSuccessTarget;

#[async_trait]
impl PublishTarget for MockBatchSuccessTarget {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        Ok(PublishedArtifact {
            local_path: None,
            commit_sha: Some("single-commit-sha".to_string()),
            remote_target: Some(format!("github://owner/repo/main/{}", report.relative_path)),
        })
    }

    async fn publish_many(
        &self,
        reports: &[RenderedReport],
    ) -> Result<PublishedBatchArtifact, PublishError> {
        Ok(PublishedBatchArtifact {
            commit_sha: Some("batch-commit-sha".to_string()),
            artifacts: reports
                .iter()
                .map(|report| PublishedArtifact {
                    local_path: None,
                    commit_sha: Some("batch-commit-sha".to_string()),
                    remote_target: Some(format!(
                        "github://owner/repo/main/{}",
                        report.relative_path
                    )),
                })
                .collect(),
        })
    }
}

struct MockRateLimitTarget;

#[async_trait]
impl PublishTarget for MockRateLimitTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        Err(PublishError::rate_limit(
            OffsetDateTime::from_unix_timestamp(1_800).unwrap(),
        ))
    }
}

struct MockAuthFailTarget;

#[async_trait]
impl PublishTarget for MockAuthFailTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        Err(PublishError::auth("bad token"))
    }
}

struct NeverCompletesTarget;

struct StalledPublishStart {
    starts_before_stall: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl rss_ai_news_storage::RunEventRepository for StalledPublishStart {
    async fn insert(
        &self,
        event: &rss_ai_news_storage::NewRunEvent,
    ) -> Result<i64, rss_ai_news_storage::StorageError> {
        if event.event_kind == "publish_started"
            && self
                .starts_before_stall
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_err()
        {
            std::future::pending().await
        } else {
            Ok(1)
        }
    }
}

#[tokio::test]
async fn publish_remote_deadline_also_bounds_preparation() {
    for batch in [false, true] {
        let (_dir, pool) = make_test_pool().await;
        let first = seed_stored_local_publish_record(&pool).await;
        let second = if batch {
            Some(seed_stored_local_publish_record(&pool).await)
        } else {
            None
        };
        let mut app = app_config(RetentionPolicy::Always, 1);
        app.lease.publish_duration_seconds = 1;
        let mut deps = publish_deps(
            pool.clone(),
            Arc::new(app),
            Arc::new(LocalFsTarget::new(std::env::temp_dir())),
            Some(Arc::new(MockSuccessTarget)),
        );
        deps.event_repo = Arc::new(StalledPublishStart {
            // In a batch, stall after one snapshot has already been prepared.
            starts_before_stall: std::sync::atomic::AtomicUsize::new(usize::from(batch)),
        });
        let flow = PublishFlow::new(Arc::new(deps));
        let outcomes = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            if let Some(second) = second {
                flow.publish_remote_batch(PublishRemoteBatchOptions {
                    items: vec![
                        batch_item(first, "AI", "Daily AI"),
                        batch_item(second, "ML", "Daily ML"),
                    ],
                })
                .await
                .items
            } else {
                vec![flow.publish_remote(remote_opts()).await]
            }
        })
        .await
        .expect("preparation must obey the same deadline as the remote request");
        assert_eq!(outcomes.len(), if batch { 2 } else { 1 });
        for item in outcomes {
            assert!(
                matches!(item.status, PublishRemoteStatus::Failed { error_kind } if error_kind == "remote_timeout")
            );
            assert_record_state(&pool, item.publish_record_id, "stored_local").await;
            assert_referenced_articles_state(&pool, item.publish_record_id, "ready_for_publish")
                .await;
            let owner: Option<String> =
                sqlx::query_scalar("SELECT lease_owner FROM publish_records WHERE id = ?")
                    .bind(item.publish_record_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(owner.is_none());
        }
    }
}

#[async_trait]
impl PublishTarget for NeverCompletesTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        std::future::pending().await
    }
}

fn deadline_flow(pool: SqlitePool) -> PublishFlow {
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.lease.publish_duration_seconds = 1;
    PublishFlow::new(Arc::new(publish_deps(
        pool,
        Arc::new(app),
        Arc::new(LocalFsTarget::new(std::env::temp_dir())),
        Some(Arc::new(NeverCompletesTarget)),
    )))
}

#[tokio::test]
async fn publish_remote_deadline_releases_retryable_claim() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local_publish_record(&pool).await;
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        deadline_flow(pool.clone()).publish_remote(remote_opts()),
    )
    .await
    .expect("remote target must stop before its one-second lease expires");
    assert!(
        matches!(outcome.status, PublishRemoteStatus::Failed { error_kind } if error_kind == "remote_timeout")
    );
    assert_record_state(&pool, id, "stored_local").await;
    assert_last_error_kind(&pool, id, "remote_timeout").await;
    assert_referenced_articles_state(&pool, id, "ready_for_publish").await;
    let owner: Option<String> =
        sqlx::query_scalar("SELECT lease_owner FROM publish_records WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(owner.is_none());
}

#[tokio::test]
async fn publish_remote_batch_deadline_releases_all_retryable_claims() {
    let (_dir, pool) = make_test_pool().await;
    let first = seed_stored_local_publish_record(&pool).await;
    let second = seed_stored_local_publish_record(&pool).await;
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        deadline_flow(pool.clone()).publish_remote_batch(PublishRemoteBatchOptions {
            items: vec![
                batch_item(first, "AI", "Daily AI"),
                batch_item(second, "ML", "Daily ML"),
            ],
        }),
    )
    .await
    .expect("batch shares one deadline before its lease expires");
    assert_eq!(outcome.items.len(), 2);
    for item in outcome.items {
        assert!(
            matches!(item.status, PublishRemoteStatus::Failed { error_kind } if error_kind == "remote_timeout")
        );
        assert_record_state(&pool, item.publish_record_id, "stored_local").await;
        assert_referenced_articles_state(&pool, item.publish_record_id, "ready_for_publish").await;
        let owner: Option<String> =
            sqlx::query_scalar("SELECT lease_owner FROM publish_records WHERE id = ?")
                .bind(item.publish_record_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(owner.is_none());
    }
}

// === B4: the configured target must match the record's stored target ===

/// A remote target that declares an identity (like `GitHubTarget`) and counts
/// how many reports were actually sent.
struct IdentifiedTarget {
    identity: &'static str,
    sent: std::sync::atomic::AtomicUsize,
}

impl IdentifiedTarget {
    fn new(identity: &'static str) -> Arc<Self> {
        Arc::new(Self {
            identity,
            sent: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn sent(&self) -> usize {
        self.sent.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl PublishTarget for IdentifiedTarget {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(PublishedArtifact {
            local_path: None,
            commit_sha: Some("identified-sha".to_string()),
            remote_target: Some(format!(
                "github://{}/{}",
                self.identity, report.relative_path
            )),
        })
    }

    fn target_identity(&self) -> Option<String> {
        Some(self.identity.to_string())
    }
}

async fn seed_stored_local_for_target(pool: &SqlitePool, remote_target: &str) -> i64 {
    let id = seed_stored_local_publish_record(pool).await;
    sqlx::query("UPDATE publish_records SET remote_target = ? WHERE id = ?")
        .bind(remote_target)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn publish_remote_rejects_record_frozen_for_another_target_without_sending() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_for_target(&pool, "old-owner/old-repo:main").await;
    let target = IdentifiedTarget::new("new-owner/new-repo:main");
    let flow = flow(pool.clone(), Some(target.clone()));

    let outcome = flow.publish_remote_record(record_id, remote_opts()).await;

    assert_eq!(
        outcome.status,
        PublishRemoteStatus::TargetMismatch {
            stored: Some("old-owner/old-repo:main".to_string()),
            current: "new-owner/new-repo:main".to_string(),
        }
    );
    assert_eq!(target.sent(), 0, "no request may reach the new target");
    assert_record_state(&pool, record_id, "failed").await;
    assert_last_error_kind(&pool, record_id, "remote_target_mismatch").await;
    let last_error: String =
        sqlx::query_scalar("SELECT last_error FROM publish_records WHERE id = ?")
            .bind(record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        last_error.contains("old-owner/old-repo:main"),
        "{last_error}"
    );
    assert!(
        last_error.contains("new-owner/new-repo:main"),
        "{last_error}"
    );
    assert_referenced_articles_state(&pool, record_id, "ready_for_publish").await;
}

#[tokio::test]
async fn publish_remote_sends_record_whose_stored_target_matches() {
    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let target = IdentifiedTarget::new("owner/repo:main");
    let flow = flow(pool.clone(), Some(target.clone()));

    let outcome = flow.publish_remote_record(record_id, remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(target.sent(), 1);
    assert_record_state(&pool, record_id, "published_remote").await;
}

#[tokio::test]
async fn publish_remote_batch_skips_only_the_mismatched_record() {
    let (_dir, pool) = make_test_pool().await;
    let drifted = seed_stored_local_for_target(&pool, "owner/repo:old-branch").await;
    let matching = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let target = IdentifiedTarget::new("owner/repo:main");
    let flow = flow(pool.clone(), Some(target.clone()));

    let outcome = flow
        .publish_remote_batch(PublishRemoteBatchOptions {
            items: vec![
                batch_item(drifted, "AI", "Daily AI"),
                batch_item(matching, "ML", "Daily ML"),
            ],
        })
        .await;

    let status_of = |id: i64| {
        outcome
            .items
            .iter()
            .find(|item| item.publish_record_id == id)
            .map(|item| item.status.clone())
            .unwrap()
    };
    assert!(matches!(
        status_of(drifted),
        PublishRemoteStatus::TargetMismatch { .. }
    ));
    assert_eq!(status_of(matching), PublishRemoteStatus::PublishedRemote);
    assert_eq!(target.sent(), 1, "only the matching report is sent");
    assert_record_state(&pool, drifted, "failed").await;
    assert_record_state(&pool, matching, "published_remote").await;
}

// === P-S16: the documented recovery for a second 422 matches the code ===

/// The batch retries a non-fast-forward 422 once; a second 422 is a
/// permanent `github_api_error`, so both records end `failed` (not
/// `stored_local`) and their articles stay unpromoted.
#[tokio::test]
async fn publish_remote_batch_second_non_fast_forward_fails_the_records() {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let (_dir, pool) = make_test_pool().await;
    let first = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let second = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/git/ref/heads/main$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object": {"sha": "head"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/git/commits/head$"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"tree": {"sha": "tree"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/owner/repo/contents/"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({"message": "Not Found"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("^/repos/owner/repo/git/(trees|commits)$"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"sha": "new"})))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .respond_with(
            ResponseTemplate::new(422)
                .set_body_json(serde_json::json!({"message": "Update is not a fast forward"})),
        )
        .mount(&server)
        .await;
    let target = rss_ai_news_publish::GitHubTarget::with_base_uri(
        rss_ai_news_publish::GitHubTargetConfig {
            token: rss_ai_news_domain::SecretString::from("t"),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: "main".to_string(),
            path_prefix: "reports".to_string(),
            commit_message_prefix: "auto: publish".to_string(),
        },
        &server.uri(),
    )
    .unwrap();
    let flow = flow(pool.clone(), Some(Arc::new(target)));

    let outcome = flow
        .publish_remote_batch(PublishRemoteBatchOptions {
            items: vec![
                batch_item(first, "AI", "Daily AI"),
                batch_item(second, "ML", "Daily ML"),
            ],
        })
        .await;

    let patches = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == "PATCH")
        .count();
    assert_eq!(patches, 2, "one retry after the first 422");
    for item in outcome.items {
        assert!(
            matches!(&item.status, PublishRemoteStatus::Failed { error_kind } if error_kind == "github_api_error"),
            "{:?}",
            item.status
        );
        assert_record_state(&pool, item.publish_record_id, "failed").await;
        assert_last_error_kind(&pool, item.publish_record_id, "github_api_error").await;
        assert_referenced_articles_state(&pool, item.publish_record_id, "ready_for_publish").await;
    }
}

// === P-S12: a rate limit's reset time gates the next claim ===

/// Rate-limited until `reset_at` on the first call, succeeds afterwards.
struct RateLimitedOnce {
    reset_at: OffsetDateTime,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl PublishTarget for RateLimitedOnce {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Err(PublishError::rate_limit(self.reset_at));
        }
        MockSuccessTarget.publish(report).await
    }
}

#[tokio::test]
async fn publish_remote_rate_limit_persists_the_reset_and_skips_claims_until_then() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local_publish_record(&pool).await;
    let before = OffsetDateTime::now_utc();
    let target = Arc::new(RateLimitedOnce {
        reset_at: before + time::Duration::minutes(10),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let flow = flow(pool.clone(), Some(target.clone()));

    let first = flow.publish_remote_record(id, remote_opts()).await;
    assert!(
        matches!(first.status, PublishRemoteStatus::Failed { error_kind } if error_kind == "github_rate_limit")
    );
    let wait: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT next_attempt_at FROM publish_records WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let wait = wait.expect("the reset time is persisted");
    assert!(wait >= before + time::Duration::minutes(10), "{wait}");

    // A rerun inside the window does not claim (and does not call GitHub).
    let second = flow.publish_remote_record(id, remote_opts()).await;
    assert_eq!(second.status, PublishRemoteStatus::NothingToClaim);
    assert_eq!(
        target.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no request inside the rate-limit window"
    );

    // Once the window has passed the record publishes.
    sqlx::query("UPDATE publish_records SET next_attempt_at = ? WHERE id = ?")
        .bind(OffsetDateTime::now_utc() - time::Duration::seconds(1))
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let third = flow.publish_remote_record(id, remote_opts()).await;
    assert_eq!(third.status, PublishRemoteStatus::PublishedRemote);
}

/// A reset far in the future is clamped to one hour: the record keeps owning
/// its articles, so it must not be parked for longer.
#[tokio::test]
async fn publish_remote_rate_limit_wait_is_clamped_to_one_hour() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local_publish_record(&pool).await;
    let target = Arc::new(RateLimitedOnce {
        reset_at: OffsetDateTime::now_utc() + time::Duration::days(30),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let flow = flow(pool.clone(), Some(target));

    flow.publish_remote_record(id, remote_opts()).await;

    let wait: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT next_attempt_at FROM publish_records WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let wait = wait.unwrap();
    assert!(
        wait <= OffsetDateTime::now_utc() + time::Duration::hours(1),
        "{wait}"
    );
}

// === P-B6: persisted diagnostics never carry server-echoed text ===

#[tokio::test]
async fn github_failure_persists_only_sanitized_reason_and_valid_request_id() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    const TOKEN: &str = "runtime-diagnostics-test-token";
    const MARKER: &str = "ECHOED-REPORT-MARKER-91c2";

    let (_dir, pool) = make_test_pool().await;
    let record_id = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("x-github-request-id", "828A:4A44E:40DA1B1:CEE6757:6B01CE59")
                .set_body_json(serde_json::json!({
                    "message": format!("Bad request: {MARKER} for {TOKEN}")
                })),
        )
        .mount(&server)
        .await;
    let target = rss_ai_news_publish::GitHubTarget::with_base_uri(
        rss_ai_news_publish::GitHubTargetConfig {
            token: rss_ai_news_domain::SecretString::from(TOKEN),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: "main".to_string(),
            path_prefix: "reports".to_string(),
            commit_message_prefix: "auto: publish".to_string(),
        },
        &server.uri(),
    )
    .unwrap();
    let flow = flow(pool.clone(), Some(Arc::new(target)));

    let outcome = flow.publish_remote_record(record_id, remote_opts()).await;

    assert!(matches!(outcome.status, PublishRemoteStatus::Failed { .. }));
    let last_error: String =
        sqlx::query_scalar("SELECT last_error FROM publish_records WHERE id = ?")
            .bind(record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let events: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT message, context_json FROM run_events WHERE event_kind = 'publish_failed'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    let persisted = format!("{last_error} {events:?}");
    assert!(last_error.contains("GET contents"), "{last_error}");
    assert!(last_error.contains("bad request"), "{last_error}");
    assert!(
        last_error.contains("828A:4A44E:40DA1B1:CEE6757:6B01CE59"),
        "{last_error}"
    );
    assert!(!persisted.contains(MARKER), "{persisted}");
    assert!(!persisted.contains(TOKEN), "{persisted}");
    // R1-P-S1: the event carries the request context as fields.
    let payload: serde_json::Value =
        serde_json::from_str(events[0].1.as_deref().expect("event payload")).unwrap();
    assert_eq!(payload["operation"], "GET contents");
    assert_eq!(payload["http_status"], 400);
    assert_eq!(payload["request_id"], "828A:4A44E:40DA1B1:CEE6757:6B01CE59");
}

// === P-B2: a write cut off by the deadline is verified before release ===

/// The write never answers within the lease (its response is lost), but the
/// destination already holds the content when asked read-only afterwards.
struct LandedButUnansweredTarget {
    landed: bool,
}

#[async_trait]
impl PublishTarget for LandedButUnansweredTarget {
    async fn publish(&self, _report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        std::future::pending().await
    }

    async fn find_published(
        &self,
        reports: &[RenderedReport],
    ) -> Result<Option<PublishedBatchArtifact>, PublishError> {
        Ok(self.landed.then(|| PublishedBatchArtifact {
            commit_sha: Some("landed-head".to_string()),
            artifacts: reports
                .iter()
                .map(|report| PublishedArtifact {
                    local_path: None,
                    commit_sha: Some("landed-head".to_string()),
                    remote_target: Some(format!(
                        "github://owner/repo/main/{}",
                        report.relative_path
                    )),
                })
                .collect(),
        }))
    }
}

/// One attempt per stage, one-second lease: without the post-timeout check
/// the last attempt would fail the record although the write landed.
fn last_attempt_flow(pool: SqlitePool, landed: bool) -> PublishFlow {
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.lease.publish_duration_seconds = 1;
    app.retry.publish_max_attempts = 1;
    PublishFlow::new(Arc::new(publish_deps(
        pool,
        Arc::new(app),
        Arc::new(LocalFsTarget::new(std::env::temp_dir())),
        Some(Arc::new(LandedButUnansweredTarget { landed })),
    )))
}

#[tokio::test]
async fn publish_remote_timeout_on_last_attempt_records_success_when_head_holds_the_report() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local_publish_record(&pool).await;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        last_attempt_flow(pool.clone(), true).publish_remote_record(id, remote_opts()),
    )
    .await
    .expect("verification stays inside the lease");

    assert_eq!(outcome.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(outcome.commit_sha.as_deref(), Some("landed-head"));
    assert_record_state(&pool, id, "published_remote").await;
    assert_referenced_articles_state(&pool, id, "published").await;
}

#[tokio::test]
async fn publish_remote_timeout_without_the_report_at_head_keeps_the_timeout() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_stored_local_publish_record(&pool).await;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        last_attempt_flow(pool.clone(), false).publish_remote_record(id, remote_opts()),
    )
    .await
    .expect("verification stays inside the lease");

    assert!(
        matches!(outcome.status, PublishRemoteStatus::Failed { error_kind } if error_kind == "remote_timeout")
    );
    // Last attempt of the stage: the fold makes the unverified record failed.
    assert_record_state(&pool, id, "failed").await;
    assert_last_error_kind(&pool, id, "remote_timeout").await;
}

#[tokio::test]
async fn publish_remote_batch_timeout_records_success_when_head_holds_the_batch() {
    let (_dir, pool) = make_test_pool().await;
    let first = seed_stored_local_publish_record(&pool).await;
    let second = seed_stored_local_publish_record(&pool).await;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        last_attempt_flow(pool.clone(), true).publish_remote_batch(PublishRemoteBatchOptions {
            items: vec![
                batch_item(first, "AI", "Daily AI"),
                batch_item(second, "ML", "Daily ML"),
            ],
        }),
    )
    .await
    .expect("verification stays inside the lease");

    assert_eq!(outcome.commit_sha.as_deref(), Some("landed-head"));
    for item in outcome.items {
        assert_eq!(item.status, PublishRemoteStatus::PublishedRemote);
        assert_record_state(&pool, item.publish_record_id, "published_remote").await;
    }
}

// === P-B1: legacy duplicate ownership is stopped before any external write ===

/// Puts `from`'s first article into `into`'s snapshot too, the state a
/// pre-0008 database can hold (two dates' snapshots sharing one article).
async fn share_first_article(pool: &SqlitePool, from: i64, into: i64) -> i64 {
    let article_id: i64 = sqlx::query_scalar(
        "SELECT article_id FROM publish_items WHERE publish_record_id = ? ORDER BY position LIMIT 1",
    )
    .bind(from)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO publish_items (publish_record_id, position, article_id, frozen_title, \
         frozen_summary, frozen_tags_json, frozen_canonical_link, frozen_source_display_name) \
         VALUES (?, 2, ?, 'Shared', 's', '[]', 'https://example.com/shared', 'Source')",
    )
    .bind(into)
    .bind(article_id)
    .execute(pool)
    .await
    .unwrap();
    article_id
}

async fn record_counters(pool: &SqlitePool, id: i64) -> (i64, i64, Option<String>) {
    sqlx::query_as(
        "SELECT attempt_count, stage_attempt_count, lease_owner FROM publish_records WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn publish_remote_stops_the_higher_record_sharing_an_article_before_sending() {
    let (_dir, pool) = make_test_pool().await;
    let keeper = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let loser = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let shared = share_first_article(&pool, keeper, loser).await;
    let target = IdentifiedTarget::new("owner/repo:main");
    let flow = flow(pool.clone(), Some(target.clone()));

    let outcome = flow.publish_remote_record(loser, remote_opts()).await;

    assert_eq!(
        outcome.status,
        PublishRemoteStatus::ArticleConflict { article_id: shared }
    );
    assert_eq!(target.sent(), 0, "the losing record must not be sent");
    assert_record_state(&pool, loser, "stored_local").await;
    assert_last_error_kind(&pool, loser, "article_owned_by_active_snapshot").await;
    let last_error: String =
        sqlx::query_scalar("SELECT last_error FROM publish_records WHERE id = ?")
            .bind(loser)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        last_error.contains(&format!("publish record {keeper}")),
        "{last_error}"
    );
    assert_eq!(
        record_counters(&pool, loser).await,
        (0, 0, None),
        "lease released and claim refunded"
    );

    // The lower record keeps the article and publishes normally.
    let kept = flow.publish_remote_record(keeper, remote_opts()).await;
    assert_eq!(kept.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(target.sent(), 1);
}

/// A published owner wins even with the higher id: its articles are already
/// `published`, so the lower record could only write and then fail.
#[tokio::test]
async fn publish_remote_stops_a_lower_record_whose_article_a_published_record_holds() {
    let (_dir, pool) = make_test_pool().await;
    let lower = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let published = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let shared = share_first_article(&pool, published, lower).await;
    sqlx::query("UPDATE publish_records SET state = 'published_remote' WHERE id = ?")
        .bind(published)
        .execute(&pool)
        .await
        .unwrap();
    let target = IdentifiedTarget::new("owner/repo:main");
    let flow = flow(pool.clone(), Some(target.clone()));

    let outcome = flow.publish_remote_record(lower, remote_opts()).await;

    assert_eq!(
        outcome.status,
        PublishRemoteStatus::ArticleConflict { article_id: shared }
    );
    assert_eq!(target.sent(), 0);
    assert_record_state(&pool, lower, "stored_local").await;
}

#[tokio::test]
async fn publish_remote_batch_drops_only_the_record_losing_a_shared_article() {
    let (_dir, pool) = make_test_pool().await;
    let keeper = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let loser = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    share_first_article(&pool, keeper, loser).await;
    let target = IdentifiedTarget::new("owner/repo:main");
    let flow = flow(pool.clone(), Some(target.clone()));

    let outcome = flow
        .publish_remote_batch(PublishRemoteBatchOptions {
            items: vec![
                batch_item(keeper, "AI", "Daily AI"),
                batch_item(loser, "ML", "Daily ML"),
            ],
        })
        .await;

    let status_of = |id: i64| {
        outcome
            .items
            .iter()
            .find(|item| item.publish_record_id == id)
            .map(|item| item.status.clone())
            .unwrap()
    };
    assert_eq!(status_of(keeper), PublishRemoteStatus::PublishedRemote);
    assert!(matches!(
        status_of(loser),
        PublishRemoteStatus::ArticleConflict { .. }
    ));
    assert_eq!(target.sent(), 1, "only the keeper's report is sent");
    assert_record_state(&pool, keeper, "published_remote").await;
    assert_record_state(&pool, loser, "stored_local").await;
}

/// Once the keeper fails, its ownership is released and the other record may
/// publish.
#[tokio::test]
async fn publish_remote_sends_the_other_record_after_the_keeper_failed() {
    let (_dir, pool) = make_test_pool().await;
    let keeper = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    let other = seed_stored_local_for_target(&pool, "owner/repo:main").await;
    share_first_article(&pool, keeper, other).await;
    sqlx::query("UPDATE publish_records SET state = 'failed' WHERE id = ?")
        .bind(keeper)
        .execute(&pool)
        .await
        .unwrap();
    let target = IdentifiedTarget::new("owner/repo:main");
    let flow = flow(pool.clone(), Some(target.clone()));

    let outcome = flow.publish_remote_record(other, remote_opts()).await;

    assert_eq!(outcome.status, PublishRemoteStatus::PublishedRemote);
    assert_eq!(target.sent(), 1);
}
