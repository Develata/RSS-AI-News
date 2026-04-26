mod common;

use rss_ai_news_storage::{NewPublishRecord, PublishRecordRepository, SqlitePublishRecordRepo};

use common::{insert_rule, make_test_pool};

#[tokio::test]
async fn publish_record_idempotency_key_duplicate_returns_none() {
    let (_dir, pool) = make_test_pool().await;
    let rule_id = insert_rule(&pool, "render", "v1", "render-sha").await;
    let policy_id = insert_rule(&pool, "selection_policy", "v1", "policy-sha").await;
    let repo = SqlitePublishRecordRepo::new(pool);
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
    let repo = SqlitePublishRecordRepo::new(pool);
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
