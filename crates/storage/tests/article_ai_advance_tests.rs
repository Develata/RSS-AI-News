mod common;

use rss_ai_news_storage::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepo, ArticleAiResultRepository,
    ClaimRequest, NewAiResult, lease_expires_at,
};
use time::{Duration, OffsetDateTime};

use common::{insert_rule, make_test_pool, seed_article};

#[tokio::test]
async fn insert_pending_and_advance_advances_article_to_ai_pending() {
    let (_dir, pool) = make_test_pool().await;
    let (prompt_version, _entry_id, article_id) = seed_article(&pool).await;
    let output_schema_version = insert_rule(
        &pool,
        "ai_output_schema",
        "schema-advance-1",
        "schema-sha-1",
    )
    .await;
    let repo = ArticleAiResultRepo::new(pool.clone());

    let outcome = repo
        .insert_pending_and_advance_article(
            &new_ai_result(article_id, prompt_version, output_schema_version, "model-a"),
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("insert and advance should succeed");

    assert!(outcome.ai_result_id.is_some());
    assert!(outcome.article_advanced);
    assert!(!outcome.article_already_advanced);
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");
}

#[tokio::test]
async fn insert_pending_returns_none_on_unique_conflict_and_detects_article_already_advanced() {
    let (_dir, pool) = make_test_pool().await;
    let (prompt_version, _entry_id, article_id) = seed_article(&pool).await;
    let output_schema_version = insert_rule(
        &pool,
        "ai_output_schema",
        "schema-advance-2",
        "schema-sha-2",
    )
    .await;
    let repo = ArticleAiResultRepo::new(pool.clone());
    let item = new_ai_result(article_id, prompt_version, output_schema_version, "model-a");
    let first = repo
        .insert_pending_and_advance_article(&item, OffsetDateTime::now_utc())
        .await
        .expect("first insert should succeed");
    assert!(first.ai_result_id.is_some());

    let second = repo
        .insert_pending_and_advance_article(&item, OffsetDateTime::now_utc())
        .await
        .expect("duplicate insert should not error");

    assert!(second.ai_result_id.is_none());
    assert!(!second.article_advanced);
    assert!(second.article_already_advanced);
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");
}

#[tokio::test]
async fn release_success_keep_true_high_score_advances_to_ready_for_publish() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, article_id, ai_result_id, owner) =
        setup_claimed_ai_result(&pool, "model-high").await;

    let outcome = repo
        .release_success_and_advance_article(
            ai_result_id,
            &owner,
            success_outcome(Some(true), Some(80)),
            "test-effective-model",
            article_id,
            30,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");

    assert!(outcome.released);
    assert_eq!(
        outcome.article_advance,
        AiCompleteArticleAdvance::ReadyForPublish
    );
    assert_eq!(article_state(&pool, article_id).await, "ready_for_publish");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "succeeded");
}

#[tokio::test]
async fn release_success_keep_true_low_score_advances_to_ai_done() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, article_id, ai_result_id, owner) = setup_claimed_ai_result(&pool, "model-low").await;

    let outcome = repo
        .release_success_and_advance_article(
            ai_result_id,
            &owner,
            success_outcome(Some(true), Some(10)),
            "test-effective-model",
            article_id,
            30,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");

    assert!(outcome.released);
    assert_eq!(outcome.article_advance, AiCompleteArticleAdvance::AiDone);
    assert_eq!(article_state(&pool, article_id).await, "ai_done");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "succeeded");
}

#[tokio::test]
async fn release_success_keep_false_no_other_succeeded_advances_to_publish_skipped() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, article_id, ai_result_id, owner) =
        setup_claimed_ai_result(&pool, "model-filtered").await;

    let outcome = repo
        .release_success_and_advance_article(
            ai_result_id,
            &owner,
            success_outcome(Some(false), None),
            "test-effective-model",
            article_id,
            30,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");

    assert!(outcome.released);
    assert_eq!(
        outcome.article_advance,
        AiCompleteArticleAdvance::PublishSkipped
    );
    assert_eq!(article_state(&pool, article_id).await, "publish_skipped");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "filtered");
}

#[tokio::test]
async fn release_success_keep_false_with_other_succeeded_returns_no_change() {
    let (_dir, pool) = make_test_pool().await;
    let (repo, article_id, ai_result_id, owner) =
        setup_claimed_ai_result(&pool, "model-filtered-with-other").await;
    sqlx::query(
        r#"
        INSERT INTO article_ai_results (
            article_id, prompt_version, output_schema_version, model_id,
            state, keep_decision, importance_score
        )
        SELECT article_id, prompt_version, output_schema_version, 'model-existing',
               'succeeded', 1, 90
        FROM article_ai_results
        WHERE id = ?
        "#,
    )
    .bind(ai_result_id)
    .execute(&pool)
    .await
    .expect("existing succeeded result should insert");

    let outcome = repo
        .release_success_and_advance_article(
            ai_result_id,
            &owner,
            success_outcome(Some(false), None),
            "test-effective-model",
            article_id,
            30,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");

    assert!(outcome.released);
    assert_eq!(outcome.article_advance, AiCompleteArticleAdvance::NoChange);
    assert_eq!(article_state(&pool, article_id).await, "ai_pending");
    assert_eq!(ai_result_state(&pool, ai_result_id).await, "filtered");
}

#[tokio::test]
async fn release_writes_effective_model_id_and_keeps_anchor_model_id() {
    // W14-A P3：fallback 成功后 effective_model_id 记实际成功模型，
    // 而 model_id（幂等键，锚定首选模型）保持不变。
    let (_dir, pool) = make_test_pool().await;
    let (repo, article_id, ai_result_id, owner) =
        setup_claimed_ai_result(&pool, "anchor-primary").await;

    let outcome = repo
        .release_success_and_advance_article(
            ai_result_id,
            &owner,
            success_outcome(Some(true), Some(80)),
            "actual-fallback-model",
            article_id,
            30,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("release should succeed");

    assert!(outcome.released);
    assert_eq!(
        ai_result_model_ids(&pool, ai_result_id).await,
        (
            "anchor-primary".to_string(),
            Some("actual-fallback-model".to_string())
        ),
        "model_id 锚定主模型不变；effective_model_id 记实际成功模型"
    );
}

async fn ai_result_model_ids(
    pool: &sqlx::SqlitePool,
    ai_result_id: i64,
) -> (String, Option<String>) {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT model_id, effective_model_id FROM article_ai_results WHERE id = ?",
    )
    .bind(ai_result_id)
    .fetch_one(pool)
    .await
    .expect("ai result row should be readable")
}

async fn setup_claimed_ai_result(
    pool: &sqlx::SqlitePool,
    model_id: &str,
) -> (ArticleAiResultRepo, i64, i64, String) {
    let (prompt_version, _entry_id, article_id) = seed_article(pool).await;
    let output_schema_version = insert_rule(
        pool,
        "ai_output_schema",
        &format!("schema-{model_id}"),
        &format!("schema-sha-{model_id}"),
    )
    .await;
    let repo = ArticleAiResultRepo::new(pool.clone());
    let inserted = repo
        .insert_pending_and_advance_article(
            &new_ai_result(article_id, prompt_version, output_schema_version, model_id),
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("pending AI result should insert");
    let owner = format!("owner-{model_id}");
    let claimed = repo
        .claim_pending(
            &ClaimRequest {
                owner: owner.clone(),
                now: OffsetDateTime::now_utc(),
                lease_expires_at: lease_expires_at(
                    OffsetDateTime::now_utc(),
                    Duration::seconds(60),
                ),
                batch_size: 1,
                max_attempts: 3,
            },
            "ai",
        )
        .await
        .expect("AI result should be claimed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].id,
        inserted.ai_result_id.expect("id should exist")
    );
    (repo, article_id, claimed[0].id, owner)
}

fn new_ai_result(
    article_id: i64,
    prompt_version: i64,
    output_schema_version: i64,
    model_id: &str,
) -> NewAiResult {
    NewAiResult {
        article_id,
        prompt_version,
        output_schema_version,
        model_id: model_id.to_string(),
    }
}

fn success_outcome(keep_decision: Option<bool>, importance_score: Option<i32>) -> AiSuccessOutcome {
    AiSuccessOutcome {
        summary: "summary".to_string(),
        tags_json: "[]".to_string(),
        importance_score,
        keep_decision,
        raw_response_artifact_id: None,
        tokens_in: Some(10),
        tokens_out: Some(20),
        cost_micro_usd: None,
        latency_ms: Some(30),
    }
}

async fn article_state(pool: &sqlx::SqlitePool, article_id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM articles WHERE id = ?")
        .bind(article_id)
        .fetch_one(pool)
        .await
        .expect("article should be readable")
}

async fn ai_result_state(pool: &sqlx::SqlitePool, ai_result_id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM article_ai_results WHERE id = ?")
        .bind(ai_result_id)
        .fetch_one(pool)
        .await
        .expect("AI result should be readable")
}
