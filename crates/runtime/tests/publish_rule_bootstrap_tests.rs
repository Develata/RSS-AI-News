//! R1-P-S6 (bootstrap half): the rule ids a publish run stamps on its records
//! must belong to *active* render / selection_policy rows. A legal database
//! state — the `default` row superseded and no row active — used to make the
//! bootstrap return the superseded row's id (tag conflict read-back), so
//! publish succeeded while date-based `rebuild-report` then failed.

mod common;

use rss_ai_news_runtime::flows::publish::{PublishRuleVersions, bootstrap_publish_rule_versions};
use rss_ai_news_storage::RuleVersionRepo;
use sqlx::SqlitePool;

use common::make_test_pool;

async fn insert_rule(pool: &SqlitePool, kind: &str, tag: &str, status: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES (?, ?, 'd', 'v1', ?) RETURNING id",
    )
    .bind(kind)
    .bind(tag)
    .bind(status)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn bootstrap_refuses_a_superseded_default_rule_when_none_is_active() {
    let (_dir, pool) = make_test_pool().await;
    insert_rule(&pool, "render", "default", "superseded").await;
    let repo = RuleVersionRepo::new(pool.clone());

    let error = bootstrap_publish_rule_versions(&repo, None)
        .await
        .expect_err("a superseded render rule must not be stamped on new records");

    let message = error.to_string();
    assert!(message.contains("render"), "{message}");
    assert!(message.contains("active"), "{message}");
}

#[tokio::test]
async fn bootstrap_refuses_a_superseded_default_selection_policy() {
    let (_dir, pool) = make_test_pool().await;
    insert_rule(&pool, "selection_policy", "default", "superseded").await;
    let repo = RuleVersionRepo::new(pool.clone());

    let error = bootstrap_publish_rule_versions(&repo, None)
        .await
        .expect_err("a superseded selection policy must not be stamped");

    assert!(error.to_string().contains("selection_policy"), "{error}");
}

#[tokio::test]
async fn bootstrap_returns_the_active_rules_and_seeds_them_on_a_fresh_db() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool.clone());

    let PublishRuleVersions {
        render_version,
        selection_policy_version,
    } = bootstrap_publish_rule_versions(&repo, None)
        .await
        .expect("fresh database seeds active rules");

    for (id, kind) in [
        (render_version, "render"),
        (selection_policy_version, "selection_policy"),
    ] {
        let (row_kind, status): (String, String) =
            sqlx::query_as("SELECT kind, status FROM rule_versions WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((row_kind.as_str(), status.as_str()), (kind, "active"));
    }

    // An existing non-default active rule is used as is.
    sqlx::query("UPDATE rule_versions SET status = 'superseded' WHERE id = ?")
        .bind(render_version)
        .execute(&pool)
        .await
        .unwrap();
    let custom = insert_rule(&pool, "render", "custom", "active").await;
    let again = bootstrap_publish_rule_versions(&repo, None).await.unwrap();
    assert_eq!(again.render_version, custom);
}
