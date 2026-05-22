//! W11-P3-C-4：[`PublishRecordRepo`] + [`PublishItemRepo`] PG 分支冒烟。
//!
//! 覆盖：
//!   - `create_if_new` ON CONFLICT(idempotency_key) DO NOTHING + find_by_*
//!   - `claim_pending_for_freeze` PG FOR UPDATE SKIP LOCKED 路径
//!   - `release_advance` 跨状态推进
//!   - `release_terminal_advance_with_articles` 跨表事务（publish_records
//!     UPDATE + articles UPDATE * N）
//!   - `freeze_snapshot`（PublishItem）跨表事务
//!
//! 默认 `#[ignore]`，需要 docker。

mod common;

use std::num::NonZeroU32;

use common::pg::{PgTestContext, make_pg_test_pool};
use rss_ai_news_storage::{
    ClaimRequest, FreezeSnapshotItem, FreezeSnapshotStatus, NewPublishRecord, PublishAdvanceExtras,
    PublishItemRepo, PublishItemRepository, PublishRecordRepo, PublishRecordRepository,
    PublishState, PublishTimestampField, TerminalAdvanceStatus,
};
use time::OffsetDateTime;

fn lease_expires(now: OffsetDateTime) -> OffsetDateTime {
    now + time::Duration::minutes(5)
}

fn new_record(
    key: &str,
    category: &str,
    render_version: i64,
    selection_policy_version: i64,
) -> NewPublishRecord {
    NewPublishRecord {
        idempotency_key: key.to_string(),
        category_key: category.to_string(),
        report_date: "2026-05-17".to_string(),
        target_timezone: "UTC".to_string(),
        render_version,
        selection_policy_version,
        remote_target: None,
    }
}

/// 给 publish_records.render_version / selection_policy_version 准备一对 FK 满足的
/// rule_versions 行（kind 任意，status='superseded' 避免触发 partial unique）。
async fn seed_render_and_policy_rules(ctx: &PgTestContext, tag: &str) -> (i64, i64) {
    let render_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('render', $1, 'r', $2, 'superseded') RETURNING id",
    )
    .bind(format!("render-{tag}"))
    .bind(format!("sha-render-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let policy_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('selection_policy', $1, 'p', $2, 'superseded') RETURNING id",
    )
    .bind(format!("policy-{tag}"))
    .bind(format!("sha-policy-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    (render_id, policy_id)
}

async fn seed_article(ctx: &PgTestContext, hash: &str, uid: &str, state: &str) -> i64 {
    let rule_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('config', $1, 'c', $2, 'superseded') RETURNING id",
    )
    .bind(format!("cfg-{uid}"))
    .bind(format!("sha-cfg-{uid}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
            feed_kind, config_version) \
         VALUES ('ai', $1, 'AI Main', 'https://example.com/feed.xml', 'rss', $2) \
         RETURNING id",
    )
    .bind(format!("src-{uid}"))
    .bind(rule_id)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let entry_id: i64 = sqlx::query_scalar(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
            title_raw, discovered_at, state, dedup_decision) \
         VALUES ($1, $2, $3, $4, 'title', $5, 'pending_fetch', 'fresh') RETURNING id",
    )
    .bind(source_id)
    .bind(uid)
    .bind(format!("https://example.com/{uid}"))
    .bind(format!("hash-{uid}"))
    .bind(OffsetDateTime::now_utc())
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let ext_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', $1, 'e', $2, 'superseded') RETURNING id",
    )
    .bind(format!("ext-{uid}"))
    .bind(format!("sha-ext-{uid}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO articles (content_hash, canonical_link, title, body_text, \
            extractor_strategy, extractor_version, content_quality, origin_feed_entry_id, state) \
         VALUES ($1, 'https://example.com/a', 'title', 'body', 'readability', $2, 'high', $3, $4) \
         RETURNING id",
    )
    .bind(hash)
    .bind(ext_id)
    .bind(entry_id)
    .bind(state)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_create_if_new_then_claim_then_release_advance() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "case1").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());

    // create_if_new：首次返 Some(id)，二次返 None（ON CONFLICT DO NOTHING）
    let first = repo
        .create_if_new(&new_record("idem-1", "ai", render, policy))
        .await
        .expect("pg create_if_new first")
        .expect("first inserts");
    let second = repo
        .create_if_new(&new_record("idem-1", "ai", render, policy))
        .await
        .expect("pg create_if_new second");
    assert!(
        second.is_none(),
        "ON CONFLICT DO NOTHING returns no id on second call"
    );

    // claim_pending_for_freeze：拿到刚 INSERT 的 pending 行
    let now = OffsetDateTime::now_utc();
    let claimed = repo
        .claim_pending_for_freeze(&ClaimRequest {
            owner: "worker-A".to_string(),
            now,
            lease_expires_at: lease_expires(now),
            batch_size: 4,
            max_attempts: 5,
        })
        .await
        .expect("pg claim_pending");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, first);

    // release_advance pending → snapshot_frozen
    let advanced = repo
        .release_advance(
            first,
            "worker-A",
            PublishState::Pending,
            PublishState::SnapshotFrozen,
            PublishTimestampField::SnapshotFrozenAt,
            now,
            PublishAdvanceExtras::default(),
        )
        .await
        .expect("pg release_advance");
    assert!(advanced, "advance applied");

    let after = repo.find_by_id(first).await.unwrap().unwrap();
    assert_eq!(after.state, "snapshot_frozen");
    assert_eq!(after.lease_owner, None, "lease cleared after advance");
    assert!(after.snapshot_frozen_at.is_some());
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_release_terminal_advance_with_articles_promotes_atomically() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "case2").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());

    // 先准备 publish_record 至 stored_local 状态
    let pr_id = repo
        .create_if_new(&new_record("idem-terminal", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let now = OffsetDateTime::now_utc();
    // claim → snapshot_frozen → rendered → stored_local（手动推进 3 状态）
    let _ = repo
        .claim_pending_for_freeze(&ClaimRequest {
            owner: "w".to_string(),
            now,
            lease_expires_at: lease_expires(now),
            batch_size: 1,
            max_attempts: 5,
        })
        .await
        .unwrap();
    repo.release_advance(
        pr_id,
        "w",
        PublishState::Pending,
        PublishState::SnapshotFrozen,
        PublishTimestampField::SnapshotFrozenAt,
        now,
        PublishAdvanceExtras::default(),
    )
    .await
    .unwrap();
    let _ = repo
        .claim_frozen_for_render(&ClaimRequest {
            owner: "w".to_string(),
            now,
            lease_expires_at: lease_expires(now),
            batch_size: 1,
            max_attempts: 5,
        })
        .await
        .unwrap();
    repo.release_advance(
        pr_id,
        "w",
        PublishState::SnapshotFrozen,
        PublishState::Rendered,
        PublishTimestampField::RenderedAt,
        now,
        PublishAdvanceExtras::default(),
    )
    .await
    .unwrap();
    let _ = repo
        .claim_rendered_for_local_store(&ClaimRequest {
            owner: "w".to_string(),
            now,
            lease_expires_at: lease_expires(now),
            batch_size: 1,
            max_attempts: 5,
        })
        .await
        .unwrap();
    repo.release_advance(
        pr_id,
        "w",
        PublishState::Rendered,
        PublishState::StoredLocal,
        PublishTimestampField::LocalStoredAt,
        now,
        PublishAdvanceExtras::default(),
    )
    .await
    .unwrap();
    // claim 4: stored_local → 准备走 terminal
    let claimed_for_remote = repo
        .claim_local_for_remote_publish(&ClaimRequest {
            owner: "w".to_string(),
            now,
            lease_expires_at: lease_expires(now),
            batch_size: 1,
            max_attempts: 5,
        })
        .await
        .unwrap();
    assert_eq!(claimed_for_remote.len(), 1);

    // 准备 ready_for_publish 的两篇 article
    let a1 = seed_article(&ctx, "hash-pub-1", "uid-pub-1", "ready_for_publish").await;
    let a2 = seed_article(&ctx, "hash-pub-2", "uid-pub-2", "ready_for_publish").await;

    let outcome = repo
        .release_terminal_advance_with_articles(
            pr_id,
            "w",
            PublishState::StoredLocal,
            PublishState::PublishedLocal,
            PublishTimestampField::RemotePublishedAt,
            vec![a1, a2],
            PublishAdvanceExtras::default(),
            now,
        )
        .await
        .expect("pg release_terminal");
    assert_eq!(outcome.status, TerminalAdvanceStatus::Advanced);

    // 验证两篇 article 都进 published
    for a in [a1, a2] {
        let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
            .bind(a)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
        assert_eq!(state, "published");
    }
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_freeze_snapshot_inserts_publish_items_in_tx() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "case3").await;
    let pr_repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let item_repo = PublishItemRepo::new_with_storage(ctx.storage_pool().clone());

    let pr_id = pr_repo
        .create_if_new(&new_record("idem-freeze", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let _ = pr_repo
        .claim_pending_for_freeze(&ClaimRequest {
            owner: "frz".to_string(),
            now,
            lease_expires_at: lease_expires(now),
            batch_size: 1,
            max_attempts: 5,
        })
        .await
        .unwrap();

    let a1 = seed_article(&ctx, "hash-frz-1", "uid-frz-1", "persisted").await;
    let a2 = seed_article(&ctx, "hash-frz-2", "uid-frz-2", "persisted").await;

    let items = vec![
        FreezeSnapshotItem {
            position: 1,
            article_id: a1,
            article_ai_result_id: None,
            frozen_title: "Title 1".to_string(),
            frozen_summary: "Summary 1".to_string(),
            frozen_tags_json: "[]".to_string(),
            frozen_score: None,
            frozen_canonical_link: "https://example.com/1".to_string(),
            frozen_source_display_name: "AI Main".to_string(),
        },
        FreezeSnapshotItem {
            position: 2,
            article_id: a2,
            article_ai_result_id: None,
            frozen_title: "Title 2".to_string(),
            frozen_summary: "Summary 2".to_string(),
            frozen_tags_json: "[]".to_string(),
            frozen_score: None,
            frozen_canonical_link: "https://example.com/2".to_string(),
            frozen_source_display_name: "AI Main".to_string(),
        },
    ];

    let outcome = item_repo
        .freeze_snapshot(pr_id, "frz", items, vec![a1, a2], now)
        .await
        .expect("pg freeze_snapshot");
    assert_eq!(outcome.status, FreezeSnapshotStatus::Frozen);
    assert_eq!(outcome.item_ids.len(), 2);

    // publish_record 进 snapshot_frozen
    let pr = pr_repo.find_by_id(pr_id).await.unwrap().unwrap();
    assert_eq!(pr.state, "snapshot_frozen");
    assert!(pr.snapshot_frozen_at.is_some());

    // 两篇 article 进 ready_for_publish
    for a in [a1, a2] {
        let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
            .bind(a)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
        assert_eq!(state, "ready_for_publish");
    }

    // publish_items 按 position 排序返回
    let items = item_repo.list_by_publish_record(pr_id).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].position, 1);
    assert_eq!(items[1].position, 2);
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_select_ai_off_passthrough_handles_null_columns() {
    // 验证 NULL::BIGINT / NULL::INTEGER 类型 cast 让 sqlx decode 工作
    let ctx = make_pg_test_pool().await;
    let item_repo = PublishItemRepo::new_with_storage(ctx.storage_pool().clone());

    seed_article(&ctx, "hash-passthrough", "uid-passthrough", "persisted").await;

    let rows = item_repo
        .select_ai_off_passthrough_candidates(
            "ai",
            time::OffsetDateTime::UNIX_EPOCH,
            time::OffsetDateTime::now_utc() + time::Duration::days(365),
            NonZeroU32::new(10).unwrap(),
        )
        .await
        .expect("pg select_ai_off_passthrough_candidates");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].article_ai_result_id, None);
    assert_eq!(rows[0].importance_score, None);
    assert_eq!(rows[0].category_key, "ai");
}
