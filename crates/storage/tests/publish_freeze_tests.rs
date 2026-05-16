mod common;

use rss_ai_news_storage::{
    ClaimRequest, FreezeSnapshotItem, FreezeSnapshotStatus, NewPublishRecord, PublishItemRepo,
    PublishItemRepository, PublishRecordRepo, PublishRecordRepository, build_owner_id,
    lease_expires_at,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_rule, make_test_pool};

#[tokio::test]
async fn freeze_snapshot_inserts_items_advances_record_to_snapshot_frozen() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_publish_record(&pool).await;
    let article_id = seed_article(&pool, "freeze-ai", "ready_for_publish").await;
    let ai_id = seed_ai_row(&pool, article_id).await;
    let repo = PublishItemRepo::new(pool.clone());

    let outcome = repo
        .freeze_snapshot(
            record_id,
            &owner,
            vec![ai_item(article_id, ai_id)],
            Vec::new(),
            now(),
        )
        .await
        .expect("freeze should succeed");

    assert_eq!(outcome.status, FreezeSnapshotStatus::Frozen);
    assert_eq!(outcome.item_ids.len(), 1);
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(record_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "snapshot_frozen");
    let items = repo
        .list_by_publish_record(record_id)
        .await
        .expect("items should list");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].frozen_score.map(|score| score.get()), Some(88));
}

#[tokio::test]
async fn freeze_snapshot_promotes_ai_off_articles_persisted_to_ready_for_publish_in_same_tx() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_publish_record(&pool).await;
    let article_id = seed_article(&pool, "freeze-direct", "persisted").await;
    let repo = PublishItemRepo::new(pool.clone());

    let outcome = repo
        .freeze_snapshot(
            record_id,
            &owner,
            vec![direct_item(article_id)],
            vec![article_id],
            now(),
        )
        .await
        .expect("freeze should succeed");

    assert_eq!(outcome.status, FreezeSnapshotStatus::Frozen);
    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = ?")
        .bind(article_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "ready_for_publish");
}

#[tokio::test]
async fn freeze_snapshot_returns_publish_record_conflict_when_lease_owner_mismatched() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, _owner) = claimed_publish_record(&pool).await;
    let article_id = seed_article(&pool, "lease-conflict", "ready_for_publish").await;
    let ai_id = seed_ai_row(&pool, article_id).await;
    let repo = PublishItemRepo::new(pool.clone());

    let outcome = repo
        .freeze_snapshot(
            record_id,
            "other-owner",
            vec![ai_item(article_id, ai_id)],
            Vec::new(),
            now(),
        )
        .await
        .expect("freeze should return conflict");

    assert_eq!(outcome.status, FreezeSnapshotStatus::PublishRecordConflict);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM publish_items WHERE publish_record_id = ?")
            .bind(record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn freeze_snapshot_returns_publish_record_conflict_when_state_not_pending() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_publish_record(&pool).await;
    sqlx::query("UPDATE publish_records SET state = 'rendered' WHERE id = ?")
        .bind(record_id)
        .execute(&pool)
        .await
        .unwrap();
    let article_id = seed_article(&pool, "state-conflict", "ready_for_publish").await;
    let ai_id = seed_ai_row(&pool, article_id).await;
    let repo = PublishItemRepo::new(pool.clone());

    let outcome = repo
        .freeze_snapshot(
            record_id,
            &owner,
            vec![ai_item(article_id, ai_id)],
            Vec::new(),
            now(),
        )
        .await
        .expect("freeze should return conflict");

    assert_eq!(outcome.status, FreezeSnapshotStatus::PublishRecordConflict);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM publish_items WHERE publish_record_id = ?")
            .bind(record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn freeze_snapshot_returns_article_state_conflict_when_promote_target_already_advanced() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_publish_record(&pool).await;
    let article_id = seed_article(&pool, "article-conflict", "ready_for_publish").await;
    let repo = PublishItemRepo::new(pool.clone());

    let outcome = repo
        .freeze_snapshot(
            record_id,
            &owner,
            vec![direct_item(article_id)],
            vec![article_id],
            now(),
        )
        .await
        .expect("freeze should return article conflict");

    assert_eq!(
        outcome.status,
        FreezeSnapshotStatus::ArticleStateConflict { article_id }
    );
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(record_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "pending");
}

#[tokio::test]
async fn freeze_snapshot_with_empty_items_only_advances_record() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_publish_record(&pool).await;
    let repo = PublishItemRepo::new(pool.clone());

    let outcome = repo
        .freeze_snapshot(record_id, &owner, Vec::new(), Vec::new(), now())
        .await
        .expect("empty freeze should advance");

    assert_eq!(outcome.status, FreezeSnapshotStatus::Frozen);
    assert!(outcome.item_ids.is_empty());
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(record_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "snapshot_frozen");
}

async fn claimed_publish_record(pool: &SqlitePool) -> (i64, String) {
    let render = insert_rule(
        pool,
        "render",
        &format!("render-{}", unique()),
        "render-sha",
    )
    .await;
    let policy = insert_rule(
        pool,
        "selection_policy",
        &format!("policy-{}", unique()),
        "policy-sha",
    )
    .await;
    let record_repo = PublishRecordRepo::new(pool.clone());
    let id = record_repo
        .create_if_new(&NewPublishRecord {
            idempotency_key: format!("ai-2026-04-28-v{}", unique()),
            category_key: "ai".to_string(),
            report_date: "2026-04-28".to_string(),
            target_timezone: "Asia/Shanghai".to_string(),
            render_version: render,
            selection_policy_version: policy,
            remote_target: None,
        })
        .await
        .unwrap()
        .unwrap();
    let owner = build_owner_id();
    let claim = ClaimRequest {
        owner: owner.clone(),
        now: now(),
        lease_expires_at: lease_expires_at(now(), Duration::seconds(30)),
        batch_size: 1,
        max_attempts: 5,
    };
    let rows = record_repo.claim_pending_for_freeze(&claim).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    (id, owner)
}

async fn seed_article(pool: &SqlitePool, content_hash: &str, state: &str) -> i64 {
    let config = insert_rule(pool, "config", &format!("cfg-{content_hash}"), content_hash).await;
    let extractor = insert_rule(
        pool,
        "extractor",
        &format!("extractor-{content_hash}"),
        &format!("extractor-sha-{content_hash}"),
    )
    .await;
    let source_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind, config_version
        )
        VALUES ('ai', ?, ?, ?, 'rss', ?)
        RETURNING id
        "#,
    )
    .bind(format!("source-{content_hash}"))
    .bind(format!("Source {content_hash}"))
    .bind(format!("https://example.com/{content_hash}.xml"))
    .bind(config)
    .fetch_one(pool)
    .await
    .unwrap();
    let entry_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            summary_raw, discovered_at, state, dedup_decision
        )
        VALUES (?, ?, ?, ?, ?, 'summary', ?, 'persisted', 'fresh')
        RETURNING id
        "#,
    )
    .bind(source_id)
    .bind(format!("uid-{content_hash}"))
    .bind(format!("https://example.com/{content_hash}"))
    .bind(format!("link-{content_hash}"))
    .bind(format!("title raw {content_hash}"))
    .bind(now())
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO articles (
            content_hash, canonical_link, title, body_text, extractor_strategy,
            extractor_version, content_quality, word_count, origin_feed_entry_id, state
        )
        VALUES (?, ?, ?, 'body', 'readability', ?, 'high', 1, ?, ?)
        RETURNING id
        "#,
    )
    .bind(content_hash)
    .bind(format!("https://example.com/article/{content_hash}"))
    .bind(format!("title {content_hash}"))
    .bind(extractor)
    .bind(entry_id)
    .bind(state)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seed_ai_row(pool: &SqlitePool, article_id: i64) -> i64 {
    let prompt = insert_rule(
        pool,
        "prompt",
        &format!("prompt-{article_id}"),
        "prompt-sha",
    )
    .await;
    let schema = insert_rule(
        pool,
        "ai_output_schema",
        &format!("schema-{article_id}"),
        "schema-sha",
    )
    .await;
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO article_ai_results (
            article_id, prompt_version, output_schema_version, model_id, state,
            summary, tags_json, importance_score, keep_decision
        )
        VALUES (?, ?, ?, 'model', 'succeeded', 'summary', '["ai"]', 88, 1)
        RETURNING id
        "#,
    )
    .bind(article_id)
    .bind(prompt)
    .bind(schema)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn ai_item(article_id: i64, ai_id: i64) -> FreezeSnapshotItem {
    FreezeSnapshotItem {
        position: 1,
        article_id,
        article_ai_result_id: Some(ai_id),
        frozen_title: "title".to_string(),
        frozen_summary: "summary".to_string(),
        frozen_tags_json: "[\"ai\"]".to_string(),
        frozen_score: Some(88),
        frozen_canonical_link: "https://example.com/article".to_string(),
        frozen_source_display_name: "AI Main".to_string(),
    }
}

fn direct_item(article_id: i64) -> FreezeSnapshotItem {
    FreezeSnapshotItem {
        position: 1,
        article_id,
        article_ai_result_id: None,
        frozen_title: "title".to_string(),
        frozen_summary: "summary".to_string(),
        frozen_tags_json: "[]".to_string(),
        frozen_score: None,
        frozen_canonical_link: "https://example.com/article".to_string(),
        frozen_source_display_name: "AI Main".to_string(),
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn unique() -> i128 {
    OffsetDateTime::now_utc().unix_timestamp_nanos()
}
