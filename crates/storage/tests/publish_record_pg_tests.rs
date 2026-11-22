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
async fn pg_release_empty_snapshot_returns_record_to_pending_and_refunds_attempt() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "empty").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .create_if_new(&new_record("idem-empty", "ai", render, policy))
        .await
        .expect("create")
        .expect("inserted");
    let now = OffsetDateTime::now_utc();
    let request = ClaimRequest {
        owner: "worker-A".to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 1,
        max_attempts: 5,
    };
    let claimed = repo
        .claim_publish_by_ids(&request, PublishState::Pending, &[id])
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);

    assert!(
        !repo
            .release_empty_snapshot(id, "someone-else", now)
            .await
            .expect("foreign owner")
    );
    assert!(
        repo.release_empty_snapshot(id, "worker-A", now)
            .await
            .expect("release")
    );

    let after = repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(after.state, "pending");
    assert_eq!(after.attempt_count, 0);
    assert_eq!(after.lease_owner, None);
    assert_eq!(after.last_error_kind.as_deref(), Some("snapshot_empty"));
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_claim_by_ids_skips_row_locked_by_another_transaction() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "skip-locked").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .create_if_new(&new_record("idem-locked", "ai", render, policy))
        .await
        .expect("create")
        .expect("inserted");

    let mut holder = ctx.pg_pool().begin().await.expect("begin");
    sqlx::query("SELECT id FROM publish_records WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut *holder)
        .await
        .expect("hold row lock");

    let now = OffsetDateTime::now_utc();
    let request = ClaimRequest {
        owner: "worker-B".to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 1,
        max_attempts: 5,
    };
    // A plain UPDATE would block here until `holder` ends.
    let claimed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        repo.claim_publish_by_ids(&request, PublishState::Pending, &[id]),
    )
    .await
    .expect("claim must not wait for the row lock")
    .expect("claim");
    assert!(claimed.is_empty(), "locked row is skipped");

    holder.rollback().await.expect("release lock");
    let claimed = repo
        .claim_publish_by_ids(&request, PublishState::Pending, &[id])
        .await
        .expect("claim after release");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, id);
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

/// B8: the render timestamp is fixed at microsecond precision so it survives
/// the PostgreSQL `timestamptz` round trip unchanged; later stages read it
/// back from the claim to re-render byte-identical reports.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_claim_returns_rendered_at_unchanged_at_microsecond_precision() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "rendered-at").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .create_if_new(&new_record("idem-rendered-at", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE publish_records SET state = 'snapshot_frozen' WHERE id = $1")
        .bind(id)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let request = |owner: &str| ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 1,
        max_attempts: 5,
    };
    let claimed = repo
        .claim_publish_by_ids(&request("render"), PublishState::SnapshotFrozen, &[id])
        .await
        .unwrap();
    assert_eq!(claimed[0].rendered_at, None);
    let rendered_at = OffsetDateTime::from_unix_timestamp_nanos(1_763_700_000_123_456_000).unwrap();
    assert!(
        repo.release_advance(
            id,
            "render",
            PublishState::SnapshotFrozen,
            PublishState::Rendered,
            PublishTimestampField::RenderedAt,
            rendered_at,
            PublishAdvanceExtras::default(),
        )
        .await
        .unwrap()
    );

    let claimed = repo
        .claim_rendered_for_local_store(&request("store"))
        .await
        .unwrap();

    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].rendered_at, Some(rendered_at));
}

/// B13 parity: PostgreSQL compares `timestamptz` by instant, so a lease ending
/// 0.5 s after a whole-second `now` is live for every lease predicate (the
/// SQLite twin is `sqlite_lease_predicates_compare_instants_not_rfc3339_text`).
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_lease_predicates_treat_subsecond_lease_as_live() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "subsec").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let live = repo
        .create_if_new(&new_record("idem-subsec-live", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let exhausted = repo
        .create_if_new(&new_record("idem-subsec-exhausted", "ml", render, policy))
        .await
        .unwrap()
        .unwrap();
    let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let lease_until = now + time::Duration::milliseconds(500);
    sqlx::query(
        "UPDATE publish_records SET state = 'stored_local', lease_owner = 'holder', \
         lease_expires_at = $1, attempt_count = CASE WHEN id = $2 THEN 5 ELSE 1 END, \
         stage_attempt_count = CASE WHEN id = $2 THEN 5 ELSE 1 END \
         WHERE id IN ($2, $3)",
    )
    .bind(lease_until)
    .bind(exhausted)
    .bind(live)
    .execute(ctx.pg_pool())
    .await
    .unwrap();
    let request = ClaimRequest {
        owner: "thief".to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 4,
        max_attempts: 5,
    };

    assert_eq!(repo.reclaim_expired_leases(now).await.unwrap(), 0);
    assert!(
        repo.claim_local_for_remote_publish(&request)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.claim_publish_by_ids(&request, PublishState::StoredLocal, &[live])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(repo.terminalize_exhausted(5, now).await.unwrap(), 0);

    let later = now + time::Duration::seconds(1);
    assert_eq!(repo.terminalize_exhausted(5, later).await.unwrap(), 1);
    assert_eq!(repo.reclaim_expired_leases(later).await.unwrap(), 1);
}

/// B1 (ADR 0012) on PostgreSQL: the budget is per stage. With
/// `max_attempts = 1` a record still advances freeze → render (the advance
/// resets the stage counter), and one retryable failure in the render stage
/// exhausts that stage's budget.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_publish_budget_is_per_stage() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "stage-budget").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .create_if_new(&new_record("idem-stage-budget", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let request = |owner: &str| ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 1,
        max_attempts: 1,
    };
    assert_eq!(
        repo.claim_publish_by_ids(&request("freeze"), PublishState::Pending, &[id])
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        repo.release_advance(
            id,
            "freeze",
            PublishState::Pending,
            PublishState::SnapshotFrozen,
            PublishTimestampField::SnapshotFrozenAt,
            now,
            PublishAdvanceExtras::default(),
        )
        .await
        .unwrap()
    );
    let counters = || async {
        sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT state, attempt_count, stage_attempt_count FROM publish_records WHERE id = $1",
        )
        .bind(id)
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap()
    };
    assert_eq!(counters().await, ("snapshot_frozen".to_string(), 1, 0));

    let claimed = repo
        .claim_frozen_for_render(&request("render"))
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1, "a new stage starts with a fresh budget");
    let outcome = repo
        .release_retryable_failure(id, "render", "boom", "db_timeout", 1, now)
        .await
        .unwrap();

    assert!(outcome.released && outcome.exhausted);
    assert_eq!(counters().await, ("failed".to_string(), 2, 1));
}

/// R1-P-S15 on PostgreSQL: the category-scoped pending claim skips an older
/// pending record of another category.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_pending_claim_in_category_skips_other_categories() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "cat-claim").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let older_ai = repo
        .create_if_new(&new_record("idem-cat-ai", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let ml = repo
        .create_if_new(&new_record("idem-cat-ml", "ml", render, policy))
        .await
        .unwrap()
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let claimed = repo
        .claim_pending_for_freeze_in_category(
            &ClaimRequest {
                owner: "cat".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 1,
                max_attempts: 5,
            },
            "ml",
        )
        .await
        .unwrap();
    assert_eq!(claimed.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ml]);
    let ai_owner: Option<String> =
        sqlx::query_scalar("SELECT lease_owner FROM publish_records WHERE id = $1")
            .bind(older_ai)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
    assert_eq!(ai_owner, None);
}

/// R1-P-S12 on PostgreSQL: the rate-limit wait blocks the state-scan and
/// the by-id claim until its instant, and the next advance clears it.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_rate_limit_wait_is_honoured_by_every_claim_and_cleared_on_advance() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "rl-wait").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .create_if_new(&new_record("idem-rl-wait", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE publish_records SET state = 'stored_local' WHERE id = $1")
        .bind(id)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    let request = |owner: &str, now: OffsetDateTime| ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 1,
        max_attempts: 5,
    };
    let first = OffsetDateTime::now_utc();
    assert_eq!(
        repo.claim_local_for_remote_publish(&request("w1", first))
            .await
            .unwrap()
            .len(),
        1
    );
    let not_before = first + time::Duration::milliseconds(60_500);
    repo.release_retryable_failure_until(
        id,
        "w1",
        "rate limited",
        "github_rate_limit",
        5,
        Some(not_before),
        first,
    )
    .await
    .unwrap();

    let early = not_before - time::Duration::milliseconds(1);
    assert!(
        repo.claim_local_for_remote_publish(&request("w2", early))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.claim_publish_by_ids(&request("w2", early), PublishState::StoredLocal, &[id])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        repo.claim_publish_by_ids(&request("w3", not_before), PublishState::StoredLocal, &[id])
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        repo.release_advance(
            id,
            "w3",
            PublishState::StoredLocal,
            PublishState::PublishedRemote,
            PublishTimestampField::RemotePublishedAt,
            not_before,
            PublishAdvanceExtras::default(),
        )
        .await
        .unwrap()
    );
    let wait: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT next_attempt_at FROM publish_records WHERE id = $1")
            .bind(id)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
    assert_eq!(wait, None);
}

/// R3F-S15 on PostgreSQL: the by-id freeze claim filters the category in SQL.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_by_id_freeze_claim_requires_the_category() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "by-id-cat").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .create_if_new(&new_record("idem-by-id-cat", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let request = |owner: &str| ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 1,
        max_attempts: 5,
    };

    assert!(
        repo.claim_pending_for_freeze_by_id_in_category(&request("c1"), id, "ml")
            .await
            .unwrap()
            .is_empty()
    );
    let (attempts, lease_owner): (i64, Option<String>) =
        sqlx::query_as("SELECT attempt_count, lease_owner FROM publish_records WHERE id = $1")
            .bind(id)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
    assert_eq!((attempts, lease_owner), (0, None), "never claimed");
    assert_eq!(
        repo.claim_pending_for_freeze_by_id_in_category(&request("c2"), id, "ai")
            .await
            .unwrap()
            .len(),
        1
    );
}

/// R3F-B2 / S12 on PostgreSQL: a deferred retry keeps the state, clears the
/// lease, refunds the claim on both counters and persists the wait; a release
/// for another state changes nothing; `None` clears the wait.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_deferred_retry_keeps_state_refunds_the_claim_and_persists_the_wait() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "deferred").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .create_if_new(&new_record("idem-deferred", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE publish_records SET state = 'stored_local' WHERE id = $1")
        .bind(id)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    let request = |owner: &str, now: OffsetDateTime| ClaimRequest {
        owner: owner.to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 1,
        max_attempts: 5,
    };
    let row = |id: i64| {
        let pool = ctx.pg_pool().clone();
        async move {
            sqlx::query_as::<_, (String, i64, i64, Option<String>, Option<OffsetDateTime>)>(
                "SELECT state, attempt_count, stage_attempt_count, lease_owner, next_attempt_at \
                 FROM publish_records WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let first = OffsetDateTime::now_utc();
    assert_eq!(
        repo.claim_publish_by_ids(&request("d1", first), PublishState::StoredLocal, &[id])
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        !repo
            .release_deferred_retry(
                id,
                "d1",
                PublishState::Rendered,
                "x",
                "x",
                None,
                rss_ai_news_storage::UnverifiedWriteUpdate::Keep,
                first
            )
            .await
            .unwrap()
    );
    let not_before = first + time::Duration::milliseconds(30_500);
    assert!(
        repo.release_deferred_retry(
            id,
            "d1",
            PublishState::StoredLocal,
            "verification incomplete",
            "remote_write_unverified",
            Some(not_before),
            rss_ai_news_storage::UnverifiedWriteUpdate::Keep,
            first,
        )
        .await
        .unwrap()
    );
    let (state, attempts, stage_attempts, lease_owner, wait) = row(id).await;
    assert_eq!(
        (state.as_str(), attempts, stage_attempts, lease_owner),
        ("stored_local", 0, 0, None)
    );
    assert!(wait.is_some());
    assert!(
        repo.claim_local_for_remote_publish(&request(
            "d2",
            not_before - time::Duration::milliseconds(1)
        ))
        .await
        .unwrap()
        .is_empty()
    );
    let again = repo
        .claim_publish_by_ids(&request("d3", not_before), PublishState::StoredLocal, &[id])
        .await
        .unwrap();
    assert_eq!(again.len(), 1);
    // R4-1: the claim projection carries the kind of the last release.
    assert_eq!(
        again[0].last_error_kind.as_deref(),
        Some("remote_write_unverified")
    );
    assert!(
        repo.release_deferred_retry(
            id,
            "d3",
            PublishState::StoredLocal,
            "x",
            "y",
            None,
            rss_ai_news_storage::UnverifiedWriteUpdate::Keep,
            not_before
        )
        .await
        .unwrap()
    );
    assert_eq!(row(id).await.4, None, "None clears the wait");
    let scanned = repo
        .claim_local_for_remote_publish(&request("d4", not_before))
        .await
        .unwrap();
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].last_error_kind.as_deref(), Some("y"));
}

/// R5-2 on PostgreSQL: a deferred release records the unverified write, a
/// later one keeps it, both claims project it, and a charged retryable
/// release and a stage advance clear it.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_deferred_retry_records_keeps_and_clears_the_unverified_write() {
    use rss_ai_news_domain::dto::publish::RemoteBlob;
    use rss_ai_news_storage::UnverifiedWriteUpdate;
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "recorded").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .create_if_new(&new_record("idem-recorded", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE publish_records SET state = 'stored_local' WHERE id = $1")
        .bind(id)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    let at = OffsetDateTime::now_utc();
    let request = |owner: &str| ClaimRequest {
        owner: owner.to_string(),
        now: at,
        lease_expires_at: lease_expires(at),
        batch_size: 1,
        max_attempts: 5,
    };
    let columns = || {
        let pool = ctx.pg_pool().clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Option<String>)>(
                "SELECT unverified_write_path, unverified_write_blob_sha FROM publish_records \
                 WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let blob = RemoteBlob {
        path: "reports/ai/2026/20260517.md".to_string(),
        blob_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
    };
    async fn defer(
        repo: &PublishRecordRepo,
        id: i64,
        owner: &str,
        update: UnverifiedWriteUpdate<'_>,
        at: OffsetDateTime,
    ) {
        assert!(
            repo.release_deferred_retry(
                id,
                owner,
                PublishState::StoredLocal,
                "unverified",
                "remote_write_unverified",
                None,
                update,
                at,
            )
            .await
            .unwrap(),
            "{owner}"
        );
    }
    let blob = &blob;

    let by_id = |owner: &'static str| {
        let repo = repo.clone();
        async move {
            let claimed = repo
                .claim_publish_by_ids(&request(owner), PublishState::StoredLocal, &[id])
                .await
                .unwrap();
            assert_eq!(claimed.len(), 1, "{owner}");
            claimed.into_iter().next().unwrap()
        }
    };
    assert_eq!(by_id("p1").await.unverified_write(), None);
    defer(
        &repo,
        id,
        "p1",
        UnverifiedWriteUpdate::Record(Some(blob)),
        at,
    )
    .await;
    let scanned = repo
        .claim_local_for_remote_publish(&request("p2"))
        .await
        .unwrap();
    assert_eq!(scanned.len(), 1);
    assert_eq!(
        scanned[0].unverified_write(),
        Some(blob.clone()),
        "scan claim"
    );
    defer(&repo, id, "p2", UnverifiedWriteUpdate::Keep, at).await;
    assert_eq!(
        by_id("p3").await.unverified_write(),
        Some(blob.clone()),
        "kept"
    );
    defer(&repo, id, "p3", UnverifiedWriteUpdate::Record(None), at).await;
    assert_eq!(columns().await, (None, None), "Record(None) clears");

    by_id("p4").await;
    defer(
        &repo,
        id,
        "p4",
        UnverifiedWriteUpdate::Record(Some(blob)),
        at,
    )
    .await;
    by_id("p5").await;
    let released = repo
        .release_retryable_failure(id, "p5", "refused", "github_api_error", 5, at)
        .await
        .unwrap();
    assert!(released.released && !released.exhausted);
    assert_eq!(columns().await, (None, None), "a charged release clears");

    by_id("p6").await;
    defer(
        &repo,
        id,
        "p6",
        UnverifiedWriteUpdate::Record(Some(blob)),
        at,
    )
    .await;
    by_id("p7").await;
    let advanced = repo
        .release_terminal_advance_with_articles(
            id,
            "p7",
            PublishState::StoredLocal,
            PublishState::PublishedRemote,
            PublishTimestampField::RemotePublishedAt,
            Vec::new(),
            PublishAdvanceExtras::default(),
            at,
        )
        .await
        .unwrap();
    assert_eq!(advanced.status, TerminalAdvanceStatus::Advanced);
    assert_eq!(columns().await, (None, None), "the advance clears");
}

/// R5-3 / R5-5 on PostgreSQL: recording a write before it is sent keeps the
/// lease and is owner-guarded; the destination holder is another category's
/// record published there or unsettled there for the same target.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_pending_write_and_destination_holder() {
    use rss_ai_news_domain::dto::publish::RemoteBlob;
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "holder").await;
    let repo = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let mut ids = Vec::new();
    for key in ["idem-me", "idem-published", "idem-pending"] {
        ids.push(
            repo.create_if_new(&new_record(key, "ai", render, policy))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let (me, published, pending) = (ids[0], ids[1], ids[2]);
    sqlx::query("UPDATE publish_records SET state = 'stored_local', remote_target = 'o/r:main'")
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    let at = OffsetDateTime::now_utc();
    let request = ClaimRequest {
        owner: "w1".to_string(),
        now: at,
        lease_expires_at: lease_expires(at),
        batch_size: 1,
        max_attempts: 5,
    };
    repo.claim_publish_by_ids(&request, PublishState::StoredLocal, &[me])
        .await
        .unwrap();
    let blob = RemoteBlob {
        path: "p.md".to_string(),
        blob_sha: "abc".to_string(),
    };
    assert!(
        !repo
            .mark_remote_write_pending(
                me,
                "other",
                PublishState::StoredLocal,
                "x",
                "k",
                Some(&blob),
                at
            )
            .await
            .unwrap()
    );
    assert!(
        repo.mark_remote_write_pending(
            me,
            "w1",
            PublishState::StoredLocal,
            "in flight",
            "remote_write_unverified",
            Some(&blob),
            at,
        )
        .await
        .unwrap()
    );
    let row: (Option<String>, Option<String>, Option<String>, i64) = sqlx::query_as(
        "SELECT lease_owner, last_error_kind, unverified_write_path, stage_attempt_count \
         FROM publish_records WHERE id = $1",
    )
    .bind(me)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            Some("w1".to_string()),
            Some("remote_write_unverified".to_string()),
            Some("p.md".to_string()),
            1
        )
    );

    let receipt = "github://o/r/main/p.md";
    let holder = || {
        repo.find_remote_destination_holder(
            me,
            "ai",
            "p.md",
            "remote_write_unverified",
            "o/r:main",
            receipt,
        )
    };
    let update = |sql: &'static str, id: i64| {
        let pool = ctx.pg_pool().clone();
        async move {
            sqlx::query(sql).bind(id).execute(&pool).await.unwrap();
        }
    };
    update(
        "UPDATE publish_records SET last_error_kind = 'remote_write_unverified', \
         unverified_write_path = 'p.md' WHERE id = $1",
        pending,
    )
    .await;
    assert_eq!(holder().await.unwrap(), None, "same category");
    update(
        "UPDATE publish_records SET category_key = 'other' WHERE id = $1",
        pending,
    )
    .await;
    assert_eq!(holder().await.unwrap(), Some(pending), "unsettled write");
    update(
        "UPDATE publish_records SET last_error_kind = NULL WHERE id = $1",
        pending,
    )
    .await;
    update(
        "UPDATE publish_records SET state = 'published_remote', category_key = 'other', \
         remote_target = 'github://o/r/main/p.md' WHERE id = $1",
        published,
    )
    .await;
    assert_eq!(holder().await.unwrap(), Some(published), "published there");
}

/// B2 (ADR 0012) on PostgreSQL under READ COMMITTED: a freeze locks its
/// articles before the ownership re-check, so it waits for a concurrent
/// freeze of the same article and then sees that snapshot's committed items
/// instead of both freezes passing the check.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_concurrent_freeze_of_the_same_article_serialises_on_the_article_lock() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "ownership").await;
    let records = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let rival = records
        .create_if_new(&new_record("idem-own-rival", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let ours = records
        .create_if_new(&new_record("idem-own-ours", "ml", render, policy))
        .await
        .unwrap()
        .unwrap();
    let now = OffsetDateTime::now_utc();
    records
        .claim_publish_by_ids(
            &ClaimRequest {
                owner: "frz-ours".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 1,
                max_attempts: 5,
            },
            PublishState::Pending,
            &[ours],
        )
        .await
        .unwrap();
    let article = seed_article(&ctx, "hash-own", "uid-own", "ready_for_publish").await;

    // The rival freeze holds the article lock and has written its items.
    let mut rival_tx = ctx.pg_pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM articles WHERE id = $1 FOR UPDATE")
        .bind(article)
        .execute(&mut *rival_tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO publish_items (publish_record_id, position, article_id, frozen_title, \
         frozen_summary, frozen_tags_json, frozen_canonical_link, frozen_source_display_name) \
         VALUES ($1, 1, $2, 't', 's', '[]', 'https://example.com', 'src')",
    )
    .bind(rival)
    .bind(article)
    .execute(&mut *rival_tx)
    .await
    .unwrap();
    sqlx::query("UPDATE publish_records SET state = 'snapshot_frozen' WHERE id = $1")
        .bind(rival)
        .execute(&mut *rival_tx)
        .await
        .unwrap();

    let items = PublishItemRepo::new_with_storage(ctx.storage_pool().clone());
    let ours_freeze = tokio::spawn(async move {
        items
            .freeze_snapshot(
                ours,
                "frz-ours",
                vec![FreezeSnapshotItem {
                    position: 1,
                    article_id: article,
                    article_ai_result_id: None,
                    frozen_title: "t".to_string(),
                    frozen_summary: "s".to_string(),
                    frozen_tags_json: "[]".to_string(),
                    frozen_score: None,
                    frozen_canonical_link: "https://example.com".to_string(),
                    frozen_source_display_name: "src".to_string(),
                }],
                Vec::new(),
                OffsetDateTime::now_utc(),
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !ours_freeze.is_finished(),
        "our freeze must wait for the rival's article lock"
    );
    rival_tx.commit().await.unwrap();

    let outcome = ours_freeze.await.unwrap().unwrap();

    assert_eq!(
        outcome.status,
        FreezeSnapshotStatus::ArticleOwnershipConflict {
            article_id: article,
            owner_record_id: rival,
        }
    );
    let (state, owned): (String, i64) = sqlx::query_as(
        "SELECT state, (SELECT COUNT(*) FROM publish_items WHERE publish_record_id = $1) \
         FROM publish_records WHERE id = $1",
    )
    .bind(ours)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    assert_eq!((state.as_str(), owned), ("pending", 0));
}

/// R1-P-S4: the freeze's ownership re-check relies on READ COMMITTED (the
/// statement after the article lock sees the winner's committed items). A
/// session whose default is REPEATABLE READ would keep the snapshot taken
/// before the wait and let both freezes commit, so the freeze sets its own
/// isolation level.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_freeze_ownership_holds_when_the_session_defaults_to_repeatable_read() {
    use sqlx::Executor;
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "own-rr").await;
    let records = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let rival = records
        .create_if_new(&new_record("idem-rr-rival", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let ours = records
        .create_if_new(&new_record("idem-rr-ours", "ml", render, policy))
        .await
        .unwrap()
        .unwrap();
    let now = OffsetDateTime::now_utc();
    records
        .claim_publish_by_ids(
            &ClaimRequest {
                owner: "frz-rr".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 1,
                max_attempts: 5,
            },
            PublishState::Pending,
            &[ours],
        )
        .await
        .unwrap();
    let article = seed_article(&ctx, "hash-rr", "uid-rr", "ready_for_publish").await;

    let repeatable_read = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _| {
            Box::pin(async move {
                connection
                    .execute(
                        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                    )
                    .await
                    .map(|_| ())
            })
        })
        .connect_with((*ctx.pg_pool().connect_options()).clone())
        .await
        .unwrap();
    let default_level: String = sqlx::query_scalar("SHOW default_transaction_isolation")
        .fetch_one(&repeatable_read)
        .await
        .unwrap();
    assert_eq!(default_level, "repeatable read");

    let mut rival_tx = ctx.pg_pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM articles WHERE id = $1 FOR UPDATE")
        .bind(article)
        .execute(&mut *rival_tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO publish_items (publish_record_id, position, article_id, frozen_title, \
         frozen_summary, frozen_tags_json, frozen_canonical_link, frozen_source_display_name) \
         VALUES ($1, 1, $2, 't', 's', '[]', 'https://example.com', 'src')",
    )
    .bind(rival)
    .bind(article)
    .execute(&mut *rival_tx)
    .await
    .unwrap();
    sqlx::query("UPDATE publish_records SET state = 'snapshot_frozen' WHERE id = $1")
        .bind(rival)
        .execute(&mut *rival_tx)
        .await
        .unwrap();

    let items = PublishItemRepo::new_with_storage(rss_ai_news_storage::StoragePool::Postgres(
        repeatable_read,
    ));
    let ours_freeze = tokio::spawn(async move {
        items
            .freeze_snapshot(
                ours,
                "frz-rr",
                vec![FreezeSnapshotItem {
                    position: 1,
                    article_id: article,
                    article_ai_result_id: None,
                    frozen_title: "t".to_string(),
                    frozen_summary: "s".to_string(),
                    frozen_tags_json: "[]".to_string(),
                    frozen_score: None,
                    frozen_canonical_link: "https://example.com".to_string(),
                    frozen_source_display_name: "src".to_string(),
                }],
                Vec::new(),
                OffsetDateTime::now_utc(),
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !ours_freeze.is_finished(),
        "our freeze must be waiting for the rival's article lock"
    );
    rival_tx.commit().await.unwrap();

    let outcome = ours_freeze.await.unwrap().unwrap();

    assert_eq!(
        outcome.status,
        FreezeSnapshotStatus::ArticleOwnershipConflict {
            article_id: article,
            owner_record_id: rival,
        }
    );
    let owners: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM publish_items WHERE article_id = $1")
            .bind(article)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
    assert_eq!(owners, 1, "exactly one snapshot owns the article");
}

/// B2 selection on PostgreSQL: an article in a non-failed snapshot is not a
/// candidate (AI-off SQL is dialect-specific).
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_ai_off_selection_excludes_article_owned_by_active_snapshot() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "own-select").await;
    let records = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let owner_record = records
        .create_if_new(&new_record("idem-own-select", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let article = seed_article(&ctx, "hash-own-sel", "uid-own-sel", "ready_for_publish").await;
    sqlx::query(
        "INSERT INTO publish_items (publish_record_id, position, article_id, frozen_title, \
         frozen_summary, frozen_tags_json, frozen_canonical_link, frozen_source_display_name) \
         VALUES ($1, 1, $2, 't', 's', '[]', 'https://example.com', 'src')",
    )
    .bind(owner_record)
    .bind(article)
    .execute(ctx.pg_pool())
    .await
    .unwrap();
    let items = PublishItemRepo::new_with_storage(ctx.storage_pool().clone());
    let now = OffsetDateTime::now_utc();
    let select = || {
        items.select_ai_off_passthrough_candidates(
            "ai",
            now - time::Duration::days(1),
            now + time::Duration::days(1),
            NonZeroU32::new(10).unwrap(),
        )
    };
    for (state, expected) in [("stored_local", 0), ("failed", 1)] {
        sqlx::query("UPDATE publish_records SET state = $1 WHERE id = $2")
            .bind(state)
            .bind(owner_record)
            .execute(ctx.pg_pool())
            .await
            .unwrap();
        assert_eq!(select().await.unwrap().len(), expected, "owner {state}");
    }
}

async fn pg_insert_item(ctx: &PgTestContext, record: i64, position: i64, article: i64) {
    sqlx::query(
        "INSERT INTO publish_items (publish_record_id, position, article_id, frozen_title, \
         frozen_summary, frozen_tags_json, frozen_canonical_link, frozen_source_display_name) \
         VALUES ($1, $2, $3, 't', 's', '[]', 'https://example.com', 'src')",
    )
    .bind(record)
    .bind(position)
    .bind(article)
    .execute(ctx.pg_pool())
    .await
    .unwrap();
}

/// R3F-P-B1 on PostgreSQL: three holders of one article, the highest id
/// published. Both stopped records name the published one as the winner;
/// once it fails, the lowest id wins and the other names it.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_three_owners_all_name_the_same_top_ranked_winner() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "own-three").await;
    let records = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let mut ids = Vec::new();
    for (key, category) in [
        ("idem-three-a", "ai"),
        ("idem-three-b", "ml"),
        ("idem-three-c", "math"),
    ] {
        ids.push(
            records
                .create_if_new(&new_record(key, category, render, policy))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let (first, second, published) = (ids[0], ids[1], ids[2]);
    let shared = seed_article(&ctx, "hash-three", "uid-three", "ready_for_publish").await;
    for id in [first, second, published] {
        pg_insert_item(&ctx, id, 1, shared).await;
    }
    let set_state = |id: i64, state: &'static str| {
        let pool = ctx.pg_pool().clone();
        async move {
            sqlx::query("UPDATE publish_records SET state = $1 WHERE id = $2")
                .bind(state)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    set_state(first, "stored_local").await;
    set_state(second, "stored_local").await;
    set_state(published, "published_remote").await;
    let items = PublishItemRepo::new_with_storage(ctx.storage_pool().clone());
    let winner = |id: i64| {
        let items = &items;
        async move {
            items
                .find_snapshot_ownership_conflict(id)
                .await
                .unwrap()
                .map(|conflict| conflict.owner_record_id)
        }
    };

    assert_eq!(winner(first).await, Some(published));
    assert_eq!(winner(second).await, Some(published));

    set_state(published, "failed").await;
    assert_eq!(winner(first).await, None);
    assert_eq!(winner(second).await, Some(first));
}

/// P-B1 on PostgreSQL: of two (legacy) snapshots sharing an article the
/// lower id keeps it unless the other is already published; a failed owner
/// keeps nothing. The release refunds the claim and keeps the stage.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_snapshot_ownership_conflict_prefers_published_then_lowest_id() {
    let ctx = make_pg_test_pool().await;
    let (render, policy) = seed_render_and_policy_rules(&ctx, "own-guard").await;
    let records = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone());
    let lower = records
        .create_if_new(&new_record("idem-guard-lower", "ai", render, policy))
        .await
        .unwrap()
        .unwrap();
    let higher = records
        .create_if_new(&new_record("idem-guard-higher", "ml", render, policy))
        .await
        .unwrap()
        .unwrap();
    let shared = seed_article(&ctx, "hash-guard", "uid-guard", "ready_for_publish").await;
    pg_insert_item(&ctx, lower, 1, shared).await;
    pg_insert_item(&ctx, higher, 1, shared).await;
    let set_state = |id: i64, state: &'static str| {
        let pool = ctx.pg_pool().clone();
        async move {
            sqlx::query("UPDATE publish_records SET state = $1 WHERE id = $2")
                .bind(state)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    set_state(lower, "stored_local").await;
    set_state(higher, "stored_local").await;
    let items = PublishItemRepo::new_with_storage(ctx.storage_pool().clone());

    assert_eq!(
        items.find_snapshot_ownership_conflict(lower).await.unwrap(),
        None
    );
    let conflict = items
        .find_snapshot_ownership_conflict(higher)
        .await
        .unwrap()
        .expect("the higher record loses");
    assert_eq!(
        (conflict.article_id, conflict.owner_record_id),
        (shared, lower)
    );

    set_state(higher, "published_remote").await;
    let conflict = items
        .find_snapshot_ownership_conflict(lower)
        .await
        .unwrap()
        .expect("a published owner wins regardless of id");
    assert_eq!(conflict.owner_record_id, higher);

    set_state(higher, "failed").await;
    assert_eq!(
        items.find_snapshot_ownership_conflict(lower).await.unwrap(),
        None
    );

    let now = OffsetDateTime::now_utc();
    let claimed = records
        .claim_publish_by_ids(
            &ClaimRequest {
                owner: "guard-owner".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 1,
                max_attempts: 5,
            },
            PublishState::StoredLocal,
            &[lower],
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert!(
        records
            .release_ownership_conflict(
                lower,
                "guard-owner",
                PublishState::StoredLocal,
                "shared",
                "article_owned_by_active_snapshot",
                now,
            )
            .await
            .unwrap()
    );
    let row: (String, i64, i64, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state, attempt_count, stage_attempt_count, lease_owner, last_error_kind \
         FROM publish_records WHERE id = $1",
    )
    .bind(lower)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "stored_local".to_string(),
            0,
            0,
            None,
            Some("article_owned_by_active_snapshot".to_string())
        )
    );
}
