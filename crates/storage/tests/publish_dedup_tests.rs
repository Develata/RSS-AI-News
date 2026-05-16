mod common;

use rss_ai_news_storage::{
    ClaimRequest, NewPublishRecord, PublishRecordRepo, PublishRecordRepository, build_owner_id,
    lease_expires_at,
};
use time::{Duration, OffsetDateTime};

use common::{insert_rule, make_test_pool};

#[tokio::test]
async fn publish_record_idempotency_key_duplicate_returns_none() {
    let (_dir, pool) = make_test_pool().await;
    let rule_id = insert_rule(&pool, "render", "v1", "render-sha").await;
    let policy_id = insert_rule(&pool, "selection_policy", "v1", "policy-sha").await;
    let repo = PublishRecordRepo::new(pool);
    let item = new_publish_record(rule_id, policy_id);

    let first = repo
        .create_if_new(&item)
        .await
        .expect("first create should succeed");
    let second = repo
        .create_if_new(&item)
        .await
        .expect("duplicate create should be idempotent");

    assert!(first.is_some());
    assert!(second.is_none());
}

#[tokio::test]
async fn publish_record_can_be_recovered_by_idempotency_key_after_duplicate() {
    let (_dir, pool) = make_test_pool().await;
    let rule_id = insert_rule(&pool, "render", "v1", "render-sha").await;
    let policy_id = insert_rule(&pool, "selection_policy", "v1", "policy-sha").await;
    let repo = PublishRecordRepo::new(pool);
    let item = new_publish_record(rule_id, policy_id);

    repo.create_if_new(&item)
        .await
        .expect("first create should succeed");
    repo.create_if_new(&item)
        .await
        .expect("duplicate create should be idempotent");

    let found = repo
        .find_by_idempotency_key(&item.idempotency_key)
        .await
        .expect("lookup should succeed")
        .expect("record should exist");

    assert_eq!(found.idempotency_key, item.idempotency_key);
    assert_eq!(found.state, "pending");
}

#[tokio::test]
async fn claim_pending_for_freeze_only_acquires_lease_keeps_state_pending() {
    let (_dir, pool) = make_test_pool().await;
    let rule_id = insert_rule(&pool, "render", "v-claim", "render-sha-claim").await;
    let policy_id = insert_rule(&pool, "selection_policy", "v-claim", "policy-sha-claim").await;
    let repo = PublishRecordRepo::new(pool);
    let item = NewPublishRecord {
        idempotency_key: "ai-2026-04-25-v-claim".to_string(),
        ..new_publish_record(rule_id, policy_id)
    };
    let id = repo
        .create_if_new(&item)
        .await
        .expect("create should succeed")
        .expect("record should be inserted");
    let now = OffsetDateTime::now_utc();
    let owner = build_owner_id();

    let claimed = repo
        .claim_pending_for_freeze(&ClaimRequest {
            owner: owner.clone(),
            now,
            lease_expires_at: lease_expires_at(now, Duration::seconds(30)),
            batch_size: 1,
            max_attempts: 5,
        })
        .await
        .expect("claim should succeed");

    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, id);
    assert_eq!(claimed[0].state, "pending");
    let found = repo
        .find_by_id(id)
        .await
        .expect("find should succeed")
        .expect("record should exist");
    assert_eq!(found.state, "pending");
    assert_eq!(found.lease_owner.as_deref(), Some(owner.as_str()));
    assert!(found.lease_expires_at.is_some());
    assert_eq!(found.attempt_count, 1);
}

fn new_publish_record(render_version: i64, selection_policy_version: i64) -> NewPublishRecord {
    NewPublishRecord {
        idempotency_key: "ai-2026-04-25-v1".to_string(),
        category_key: "ai".to_string(),
        report_date: "2026-04-25".to_string(),
        target_timezone: "Asia/Shanghai".to_string(),
        render_version,
        selection_policy_version,
        remote_target: Some("github://owner/repo/main/path".to_string()),
    }
}
