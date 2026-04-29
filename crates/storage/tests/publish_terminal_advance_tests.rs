mod common;

use rss_ai_news_storage::{
    ClaimRequest, NewPublishRecord, PublishAdvanceExtras, PublishRecordRepository, PublishState,
    PublishTimestampField, SqlitePublishRecordRepo, TerminalAdvanceStatus, build_owner_id,
    lease_expires_at,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_rule, make_test_pool};

#[tokio::test]
async fn terminal_advance_published_local_advances_record_and_promotes_articles() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_rendered_record(&pool).await;
    let article_id = seed_article(&pool, "terminal-ok", "ready_for_publish").await;
    let repo = SqlitePublishRecordRepo::new(pool.clone());

    let outcome = repo
        .release_terminal_advance_with_articles(
            record_id,
            &owner,
            PublishState::Rendered,
            PublishState::PublishedLocal,
            PublishTimestampField::LocalStoredAt,
            vec![article_id],
            PublishAdvanceExtras {
                local_path: Some("out.md".to_string()),
                ..PublishAdvanceExtras::default()
            },
            now(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, TerminalAdvanceStatus::Advanced);
    assert_record_state(&pool, record_id, "published_local").await;
    assert_article_state(&pool, article_id, "published").await;
    let local_path: Option<String> =
        sqlx::query_scalar("SELECT local_path FROM publish_records WHERE id = ?")
            .bind(record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(local_path.as_deref(), Some("out.md"));
}

#[tokio::test]
async fn publish_terminal_advance_stored_local_to_published_remote_with_articles() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_stored_local_record(&pool).await;
    let article_id = seed_article(&pool, "terminal-remote-ok", "ready_for_publish").await;
    let repo = SqlitePublishRecordRepo::new(pool.clone());

    let outcome = repo
        .release_terminal_advance_with_articles(
            record_id,
            &owner,
            PublishState::StoredLocal,
            PublishState::PublishedRemote,
            PublishTimestampField::RemotePublishedAt,
            vec![article_id],
            PublishAdvanceExtras {
                remote_target: Some("github://owner/repo/main/reports/ai.md".to_string()),
                commit_sha: Some("remote-commit-sha".to_string()),
                ..PublishAdvanceExtras::default()
            },
            now(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, TerminalAdvanceStatus::Advanced);
    assert_record_state(&pool, record_id, "published_remote").await;
    assert_article_state(&pool, article_id, "published").await;
    let row: (Option<String>, Option<String>, Option<OffsetDateTime>, Option<String>) =
        sqlx::query_as(
            "SELECT commit_sha, remote_target, remote_published_at, local_path FROM publish_records WHERE id = ?",
        )
        .bind(record_id)
        .fetch_one(&pool)
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

#[tokio::test]
async fn terminal_advance_returns_publish_record_conflict_when_lease_owner_mismatched() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, _owner) = claimed_rendered_record(&pool).await;
    let article_id = seed_article(&pool, "terminal-owner-conflict", "ready_for_publish").await;
    let repo = SqlitePublishRecordRepo::new(pool.clone());

    let outcome = repo
        .release_terminal_advance_with_articles(
            record_id,
            "other-owner",
            PublishState::Rendered,
            PublishState::PublishedLocal,
            PublishTimestampField::LocalStoredAt,
            vec![article_id],
            PublishAdvanceExtras::default(),
            now(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, TerminalAdvanceStatus::PublishRecordConflict);
    assert_record_state(&pool, record_id, "rendered").await;
    assert_article_state(&pool, article_id, "ready_for_publish").await;
}

#[tokio::test]
async fn terminal_advance_returns_publish_record_conflict_when_state_not_matching_from() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_rendered_record(&pool).await;
    sqlx::query("UPDATE publish_records SET state = 'stored_local' WHERE id = ?")
        .bind(record_id)
        .execute(&pool)
        .await
        .unwrap();
    let article_id = seed_article(&pool, "terminal-state-conflict", "ready_for_publish").await;
    let repo = SqlitePublishRecordRepo::new(pool.clone());

    let outcome = repo
        .release_terminal_advance_with_articles(
            record_id,
            &owner,
            PublishState::Rendered,
            PublishState::PublishedLocal,
            PublishTimestampField::LocalStoredAt,
            vec![article_id],
            PublishAdvanceExtras::default(),
            now(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, TerminalAdvanceStatus::PublishRecordConflict);
    assert_record_state(&pool, record_id, "stored_local").await;
    assert_article_state(&pool, article_id, "ready_for_publish").await;
}

#[tokio::test]
async fn terminal_advance_returns_article_state_conflict_when_promote_target_already_published() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_rendered_record(&pool).await;
    let article_id = seed_article(&pool, "terminal-article-conflict", "published").await;
    let repo = SqlitePublishRecordRepo::new(pool.clone());

    let outcome = repo
        .release_terminal_advance_with_articles(
            record_id,
            &owner,
            PublishState::Rendered,
            PublishState::PublishedLocal,
            PublishTimestampField::LocalStoredAt,
            vec![article_id],
            PublishAdvanceExtras::default(),
            now(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.status,
        TerminalAdvanceStatus::ArticleStateConflict { article_id }
    );
    assert_record_state(&pool, record_id, "rendered").await;
    assert_article_state(&pool, article_id, "published").await;
}

#[tokio::test]
async fn terminal_advance_with_empty_promote_ids_only_advances_record() {
    let (_dir, pool) = make_test_pool().await;
    let (record_id, owner) = claimed_rendered_record(&pool).await;
    let repo = SqlitePublishRecordRepo::new(pool.clone());

    let outcome = repo
        .release_terminal_advance_with_articles(
            record_id,
            &owner,
            PublishState::Rendered,
            PublishState::PublishedLocal,
            PublishTimestampField::LocalStoredAt,
            Vec::new(),
            PublishAdvanceExtras::default(),
            now(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, TerminalAdvanceStatus::Advanced);
    assert_record_state(&pool, record_id, "published_local").await;
}

async fn claimed_rendered_record(pool: &SqlitePool) -> (i64, String) {
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
    let repo = SqlitePublishRecordRepo::new(pool.clone());
    let id = repo
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
    sqlx::query("UPDATE publish_records SET state = 'rendered' WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    let owner = build_owner_id();
    let rows = repo
        .claim_rendered_for_local_store(&ClaimRequest {
            owner: owner.clone(),
            now: now(),
            lease_expires_at: lease_expires_at(now(), Duration::seconds(30)),
            batch_size: 1,
            max_attempts: 5,
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    (id, owner)
}

async fn claimed_stored_local_record(pool: &SqlitePool) -> (i64, String) {
    let (id, _) = claimed_rendered_record(pool).await;
    sqlx::query(
        r#"
        UPDATE publish_records
        SET state = 'stored_local',
            local_path = 'local/report.md',
            local_stored_at = ?,
            remote_target = 'github://owner/repo/main/reports/ai.md',
            lease_owner = NULL,
            lease_expires_at = NULL
        WHERE id = ?
        "#,
    )
    .bind(now())
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    let owner = build_owner_id();
    let repo = SqlitePublishRecordRepo::new(pool.clone());
    let rows = repo
        .claim_local_for_remote_publish(&ClaimRequest {
            owner: owner.clone(),
            now: now(),
            lease_expires_at: lease_expires_at(now(), Duration::seconds(30)),
            batch_size: 1,
            max_attempts: 5,
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
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
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, feed_kind, config_version) VALUES ('ai', ?, ?, ?, 'rss', ?) RETURNING id",
    )
    .bind(format!("source-{content_hash}"))
    .bind(format!("Source {content_hash}"))
    .bind(format!("https://example.com/{content_hash}.xml"))
    .bind(config)
    .fetch_one(pool)
    .await
    .unwrap();
    let entry_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, title_raw, discovered_at, state, dedup_decision) VALUES (?, ?, ?, ?, 'title', ?, 'persisted', 'fresh') RETURNING id",
    )
    .bind(source_id)
    .bind(format!("uid-{content_hash}"))
    .bind(format!("https://example.com/{content_hash}"))
    .bind(format!("link-{content_hash}"))
    .bind(now())
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO articles (content_hash, canonical_link, title, body_text, extractor_strategy, extractor_version, content_quality, word_count, origin_feed_entry_id, state) VALUES (?, ?, ?, 'body', 'readability', ?, 'high', 1, ?, ?) RETURNING id",
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

async fn assert_record_state(pool: &SqlitePool, id: i64, expected: &str) {
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, expected);
}

async fn assert_article_state(pool: &SqlitePool, id: i64, expected: &str) {
    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, expected);
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn unique() -> i128 {
    OffsetDateTime::now_utc().unix_timestamp_nanos()
}
