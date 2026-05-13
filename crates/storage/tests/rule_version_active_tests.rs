mod common;

use rss_ai_news_storage::{RuleVersionRepository, SqliteRuleVersionRepo};

use common::make_test_pool;

#[tokio::test]
async fn active_rule_returns_none_for_unknown_kind_in_empty_db() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteRuleVersionRepo::new(pool.clone());

    let active = repo
        .active_rule("nonexistent_kind")
        .await
        .expect("active_rule query should succeed");

    assert!(active.is_none());
}

#[tokio::test]
async fn active_rule_returns_seeded_active_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteRuleVersionRepo::new(pool.clone());
    let id = repo
        .get_or_create("prompt", "default", "default prompt", "sha-default")
        .await
        .expect("insert rule_versions");

    let active = repo
        .active_rule("prompt")
        .await
        .expect("active_rule query should succeed")
        .expect("active prompt rule must exist after get_or_create (default status='active')");

    assert_eq!(active.id, id);
    assert_eq!(active.kind, "prompt");
    assert_eq!(active.version_tag, "default");
    assert_eq!(active.payload_sha256, "sha-default");
    assert_eq!(active.status, "active");
    assert!(active.retired_at.is_none());
}

#[tokio::test]
async fn active_rule_skips_non_active_rows() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteRuleVersionRepo::new(pool.clone());
    repo.get_or_create("prompt", "v1", "v1 prompt", "sha-v1")
        .await
        .expect("insert v1");
    // 模拟 reindex 完成后的状态：v1 → superseded，并植入新 v2 active
    sqlx::query("UPDATE rule_versions SET status = 'superseded', retired_at = ? WHERE kind = 'prompt' AND version_tag = 'v1'")
        .bind("2026-05-12T00:00:00.000Z")
        .execute(&pool)
        .await
        .expect("rotate v1 to superseded");
    repo.get_or_create("prompt", "v2", "v2 prompt", "sha-v2")
        .await
        .expect("insert v2");

    let active = repo
        .active_rule("prompt")
        .await
        .expect("active_rule query should succeed")
        .expect("v2 should be active after rotation");

    assert_eq!(active.version_tag, "v2");
    assert_eq!(active.payload_sha256, "sha-v2");
    assert_eq!(active.status, "active");
}

#[tokio::test]
async fn active_rule_returns_none_when_all_rows_superseded() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteRuleVersionRepo::new(pool.clone());
    repo.get_or_create("prompt", "v1", "v1 prompt", "sha-v1")
        .await
        .expect("insert v1");
    sqlx::query(
        "UPDATE rule_versions SET status = 'superseded', retired_at = ? WHERE kind = 'prompt'",
    )
    .bind("2026-05-12T00:00:00.000Z")
    .execute(&pool)
    .await
    .expect("retire all prompt rows");

    let active = repo
        .active_rule("prompt")
        .await
        .expect("active_rule query should succeed");

    assert!(active.is_none());
}

#[tokio::test]
async fn partial_unique_index_rejects_second_active_row_for_same_kind() {
    let (_dir, pool) = make_test_pool().await;
    let repo = SqliteRuleVersionRepo::new(pool.clone());
    repo.get_or_create("prompt", "v1", "v1", "sha-v1")
        .await
        .expect("insert v1");

    // 直接 INSERT 第二行 status='active' 必须被 partial unique index 拒绝。
    let result = sqlx::query(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) VALUES (?, ?, ?, ?, 'active')",
    )
    .bind("prompt")
    .bind("v2")
    .bind("v2")
    .bind("sha-v2")
    .execute(&pool)
    .await;

    assert!(
        result.is_err(),
        "uq_rule_versions_kind_active 必须拒绝同 kind 的第二行 active"
    );
}
