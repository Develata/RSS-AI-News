//! W15 重试预算耗尽闭环行为锁定（sqlite；PG 经 CI migrate job + SQL 跨方言
//! 等价兜底）。
//!
//! §3 release 折叠锁定点：
//! - 最后一次预算内尝试 retryable 失败 → 直接转终态（exhausted=true）；
//! - 预算未尽 → 回可领取态（exhausted=false）；
//! - lease guard 冲突 → released=false 且行不动。
//!
//! §4 terminalize_exhausted sweep 锁定点：
//! - 仅"可领取态 + attempt_count >= max + lease 空/过期"的行转终态；
//! - 既有 last_error* 保留（COALESCE），无错误行落兜底文案；
//! - 非可领取态（running / 终态）与未耗尽行不动；
//! - 未过期 lease 行不动。

mod common;

use rss_ai_news_storage::{
    ArticleAiResultRepo, ArticleAiResultRepository, ClaimRequest, FeedEntryRepo,
    FeedEntryRepository, NewAiResult, NewPublishRecord, PublishRecordRepo, PublishRecordRepository,
    lease_expires_at,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_feed_entry, insert_rule, seed_article, seed_source};

const MAX_ATTEMPTS: u32 = 3;

fn claim_request(owner: &str) -> ClaimRequest {
    let now = OffsetDateTime::now_utc();
    ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires_at(now, Duration::seconds(60)),
        batch_size: 10,
        max_attempts: MAX_ATTEMPTS,
    }
}

// ── AiResult ───────────────────────────────────────────────────

async fn seed_ai_row(pool: &SqlitePool, article_id: i64, model: &str) -> i64 {
    let repo = ArticleAiResultRepo::new(pool.clone());
    repo.insert_pending(&NewAiResult {
        article_id,
        prompt_version: 1,
        output_schema_version: 1,
        model_id: model.to_string(),
    })
    .await
    .expect("insert pending should succeed")
    .expect("row should be new")
}

async fn set_ai_row(
    pool: &SqlitePool,
    id: i64,
    state: &str,
    attempt_count: i64,
    last_error: Option<&str>,
    lease_expires_at: Option<OffsetDateTime>,
) {
    sqlx::query(
        "UPDATE article_ai_results SET state = ?, attempt_count = ?, last_error = ?, \
         last_error_kind = ?, lease_expires_at = ? WHERE id = ?",
    )
    .bind(state)
    .bind(attempt_count)
    .bind(last_error)
    .bind(last_error.map(|_| "http_timeout"))
    .bind(lease_expires_at)
    .bind(id)
    .execute(pool)
    .await
    .expect("ai row update should succeed");
}

async fn ai_row_state(pool: &SqlitePool, id: i64) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT state, last_error, last_error_kind FROM article_ai_results WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("ai row should be readable")
}

#[tokio::test]
async fn ai_terminalize_converts_only_exhausted_pending() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_, _, article_id) = seed_article(&pool).await;
    let exhausted = seed_ai_row(&pool, article_id, "m-exhausted").await;
    let budget_left = seed_ai_row(&pool, article_id, "m-budget-left").await;
    let running = seed_ai_row(&pool, article_id, "m-running").await;

    set_ai_row(
        &pool,
        exhausted,
        "pending",
        i64::from(MAX_ATTEMPTS),
        Some("timeout from last attempt"),
        None,
    )
    .await;
    set_ai_row(&pool, budget_left, "pending", 1, None, None).await;
    set_ai_row(
        &pool,
        running,
        "running",
        i64::from(MAX_ATTEMPTS),
        None,
        Some(OffsetDateTime::now_utc() + Duration::seconds(60)),
    )
    .await;

    let repo = ArticleAiResultRepo::new(pool.clone());
    let swept = repo
        .terminalize_exhausted(MAX_ATTEMPTS, OffsetDateTime::now_utc())
        .await
        .expect("sweep should succeed");

    assert_eq!(swept, 1);
    let (state, error, kind) = ai_row_state(&pool, exhausted).await;
    assert_eq!(state, "permanent_failed");
    // COALESCE 保留 retryable release 留下的真实错误。
    assert_eq!(error.as_deref(), Some("timeout from last attempt"));
    assert_eq!(kind.as_deref(), Some("http_timeout"));
    assert_eq!(ai_row_state(&pool, budget_left).await.0, "pending");
    assert_eq!(ai_row_state(&pool, running).await.0, "running");
}

#[tokio::test]
async fn ai_terminalize_fills_fallback_error_when_absent() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_, _, article_id) = seed_article(&pool).await;
    let row = seed_ai_row(&pool, article_id, "m-no-error").await;
    set_ai_row(&pool, row, "pending", i64::from(MAX_ATTEMPTS), None, None).await;

    let repo = ArticleAiResultRepo::new(pool.clone());
    let swept = repo
        .terminalize_exhausted(MAX_ATTEMPTS, OffsetDateTime::now_utc())
        .await
        .expect("sweep should succeed");

    assert_eq!(swept, 1);
    let (state, error, kind) = ai_row_state(&pool, row).await;
    assert_eq!(state, "permanent_failed");
    assert_eq!(error.as_deref(), Some("retry budget exhausted"));
    assert_eq!(kind.as_deref(), Some("retry_budget_exhausted"));
}

#[tokio::test]
async fn ai_terminalize_skips_unexpired_lease() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_, _, article_id) = seed_article(&pool).await;
    let row = seed_ai_row(&pool, article_id, "m-leased").await;
    set_ai_row(
        &pool,
        row,
        "pending",
        i64::from(MAX_ATTEMPTS),
        None,
        Some(OffsetDateTime::now_utc() + Duration::seconds(60)),
    )
    .await;

    let repo = ArticleAiResultRepo::new(pool.clone());
    let swept = repo
        .terminalize_exhausted(MAX_ATTEMPTS, OffsetDateTime::now_utc())
        .await
        .expect("sweep should succeed");

    assert_eq!(swept, 0);
    assert_eq!(ai_row_state(&pool, row).await.0, "pending");
}

// ── §3 release 折叠 ────────────────────────────────────────────

#[tokio::test]
async fn ai_release_retryable_on_final_attempt_folds_to_permanent_failed() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_, _, article_id) = seed_article(&pool).await;
    let row = seed_ai_row(&pool, article_id, "m-final").await;
    // 预置 attempt_count = max-1：本次 claim 自增到 max，即预算内最后一次尝试。
    sqlx::query("UPDATE article_ai_results SET attempt_count = ? WHERE id = ?")
        .bind(i64::from(MAX_ATTEMPTS - 1))
        .bind(row)
        .execute(&pool)
        .await
        .expect("attempt prime should succeed");

    let repo = ArticleAiResultRepo::new(pool.clone());
    let claimed = repo
        .claim_pending(&claim_request("worker-a"), "ai")
        .await
        .expect("claim should succeed");
    assert_eq!(claimed.len(), 1);

    let outcome = repo
        .release_retryable_failure(
            row,
            "worker-a",
            "timeout",
            "http_timeout",
            MAX_ATTEMPTS,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");

    assert!(outcome.released);
    assert!(outcome.exhausted);
    let (state, error, kind) = ai_row_state(&pool, row).await;
    assert_eq!(state, "permanent_failed");
    assert_eq!(error.as_deref(), Some("timeout"));
    assert_eq!(kind.as_deref(), Some("http_timeout"));
}

#[tokio::test]
async fn ai_release_retryable_with_budget_left_returns_to_pending() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_, _, article_id) = seed_article(&pool).await;
    let row = seed_ai_row(&pool, article_id, "m-budget").await;

    let repo = ArticleAiResultRepo::new(pool.clone());
    let claimed = repo
        .claim_pending(&claim_request("worker-a"), "ai")
        .await
        .expect("claim should succeed");
    assert_eq!(claimed.len(), 1);

    let outcome = repo
        .release_retryable_failure(
            row,
            "worker-a",
            "timeout",
            "http_timeout",
            MAX_ATTEMPTS,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");

    assert!(outcome.released);
    assert!(!outcome.exhausted);
    assert_eq!(ai_row_state(&pool, row).await.0, "pending");
}

#[tokio::test]
async fn ai_release_retryable_wrong_owner_is_conflict() {
    let (_dir, pool) = common::make_test_pool().await;
    let (_, _, article_id) = seed_article(&pool).await;
    let row = seed_ai_row(&pool, article_id, "m-owner").await;

    let repo = ArticleAiResultRepo::new(pool.clone());
    repo.claim_pending(&claim_request("worker-a"), "ai")
        .await
        .expect("claim should succeed");

    let outcome = repo
        .release_retryable_failure(
            row,
            "worker-b",
            "timeout",
            "http_timeout",
            MAX_ATTEMPTS,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("wrong owner release should not error");

    assert!(!outcome.released);
    assert!(!outcome.exhausted);
    assert_eq!(ai_row_state(&pool, row).await.0, "running");
}

#[tokio::test]
async fn feed_release_retryable_on_final_attempt_folds_to_failed() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let entry_id = insert_feed_entry(&pool, source_id, "uid-fold", "hash-fold").await;
    sqlx::query("UPDATE feed_entries SET attempt_count = ? WHERE id = ?")
        .bind(i64::from(MAX_ATTEMPTS - 1))
        .bind(entry_id)
        .execute(&pool)
        .await
        .expect("attempt prime should succeed");

    let repo = FeedEntryRepo::new(pool.clone());
    let claimed = repo
        .claim_pending_fetch(&claim_request("worker-a"))
        .await
        .expect("claim should succeed");
    assert!(claimed.iter().any(|entry| entry.id == entry_id));

    let outcome = repo
        .release_retryable_failure(
            entry_id,
            "worker-a",
            "fetch timeout",
            "http_timeout",
            MAX_ATTEMPTS,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");

    assert!(outcome.released);
    assert!(outcome.exhausted);
    let state: String = sqlx::query_scalar("SELECT state FROM feed_entries WHERE id = ?")
        .bind(entry_id)
        .fetch_one(&pool)
        .await
        .expect("entry should be readable");
    assert_eq!(state, "failed");
}

#[tokio::test]
async fn publish_release_retryable_folds_only_when_exhausted() {
    let (_dir, pool) = common::make_test_pool().await;
    // 预算未尽：claim 后 attempt=1 < max，release 维持原阶段态。
    let keep = seed_publish_record(&pool, "k-fold-keep", "pending", 0).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let claimed = repo
        .claim_pending_for_freeze(&claim_request("worker-a"))
        .await
        .expect("claim should succeed");
    assert!(claimed.iter().any(|record| record.id == keep));
    let outcome = repo
        .release_retryable_failure(
            keep,
            "worker-a",
            "github 502",
            "http_5xx",
            MAX_ATTEMPTS,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");
    assert!(outcome.released);
    assert!(!outcome.exhausted);
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(keep)
        .fetch_one(&pool)
        .await
        .expect("record should be readable");
    assert_eq!(state, "pending");

    // 最后一次尝试：attempt 预置 max-1，claim 自增到 max → release 折叠 failed。
    let fold = seed_publish_record(
        &pool,
        "k-fold-final",
        "pending",
        i64::from(MAX_ATTEMPTS - 1),
    )
    .await;
    let claimed = repo
        .claim_pending_for_freeze(&claim_request("worker-b"))
        .await
        .expect("claim should succeed");
    assert!(claimed.iter().any(|record| record.id == fold));
    let outcome = repo
        .release_retryable_failure(
            fold,
            "worker-b",
            "github 502",
            "http_5xx",
            MAX_ATTEMPTS,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");
    assert!(outcome.released);
    assert!(outcome.exhausted);
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(fold)
        .fetch_one(&pool)
        .await
        .expect("record should be readable");
    assert_eq!(state, "failed");
}

// ── FeedEntry ──────────────────────────────────────────────────

#[tokio::test]
async fn feed_terminalize_converts_exhausted_pending_fetch() {
    let (_dir, pool) = common::make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let exhausted = insert_feed_entry(&pool, source_id, "uid-exhausted", "hash-exhausted").await;
    let budget_left = insert_feed_entry(&pool, source_id, "uid-left", "hash-left").await;

    sqlx::query("UPDATE feed_entries SET attempt_count = ?, last_error = 'fetch timeout', last_error_kind = 'http_timeout' WHERE id = ?")
        .bind(i64::from(MAX_ATTEMPTS))
        .bind(exhausted)
        .execute(&pool)
        .await
        .expect("attempt update should succeed");

    let repo = FeedEntryRepo::new(pool.clone());
    let swept = repo
        .terminalize_exhausted(MAX_ATTEMPTS, OffsetDateTime::now_utc())
        .await
        .expect("sweep should succeed");

    assert_eq!(swept, 1);
    let (state, error): (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error FROM feed_entries WHERE id = ?")
            .bind(exhausted)
            .fetch_one(&pool)
            .await
            .expect("entry should be readable");
    assert_eq!(state, "failed");
    assert_eq!(error.as_deref(), Some("fetch timeout"));
    let state: String = sqlx::query_scalar("SELECT state FROM feed_entries WHERE id = ?")
        .bind(budget_left)
        .fetch_one(&pool)
        .await
        .expect("entry should be readable");
    assert_eq!(state, "pending_fetch");
}

// ── PublishRecord ──────────────────────────────────────────────

async fn seed_publish_record(pool: &SqlitePool, key: &str, state: &str, attempts: i64) -> i64 {
    // render_version / selection_policy_version 是 rule_versions 外键。
    let render_id = insert_rule(
        pool,
        "render",
        &format!("render-{key}"),
        &format!("rsha-{key}"),
    )
    .await;
    let policy_id = insert_rule(
        pool,
        "selection_policy",
        &format!("policy-{key}"),
        &format!("psha-{key}"),
    )
    .await;
    let repo = PublishRecordRepo::new(pool.clone());
    let id = repo
        .create_if_new(&NewPublishRecord {
            idempotency_key: key.to_string(),
            category_key: "ai".to_string(),
            report_date: "2026-06-10".to_string(),
            target_timezone: "Asia/Shanghai".to_string(),
            render_version: render_id,
            selection_policy_version: policy_id,
            remote_target: None,
        })
        .await
        .expect("create should succeed")
        .expect("record should be new");
    sqlx::query("UPDATE publish_records SET state = ?, attempt_count = ? WHERE id = ?")
        .bind(state)
        .bind(attempts)
        .bind(id)
        .execute(pool)
        .await
        .expect("record update should succeed");
    id
}

#[tokio::test]
async fn publish_terminalize_converts_exhausted_stage_states() {
    let (_dir, pool) = common::make_test_pool().await;
    let rendered_exhausted =
        seed_publish_record(&pool, "k-rendered", "rendered", i64::from(MAX_ATTEMPTS)).await;
    let pending_left = seed_publish_record(&pool, "k-pending", "pending", 1).await;
    let published = seed_publish_record(
        &pool,
        "k-published",
        "published_remote",
        i64::from(MAX_ATTEMPTS),
    )
    .await;

    let repo = PublishRecordRepo::new(pool.clone());
    let swept = repo
        .terminalize_exhausted(MAX_ATTEMPTS, OffsetDateTime::now_utc())
        .await
        .expect("sweep should succeed");

    assert_eq!(swept, 1);
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(rendered_exhausted)
        .fetch_one(&pool)
        .await
        .expect("record should be readable");
    assert_eq!(state, "failed");
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(pending_left)
        .fetch_one(&pool)
        .await
        .expect("record should be readable");
    assert_eq!(state, "pending");
    // 成功终态不在可领取态白名单，预算满也不动。
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(published)
        .fetch_one(&pool)
        .await
        .expect("record should be readable");
    assert_eq!(state, "published_remote");
}
