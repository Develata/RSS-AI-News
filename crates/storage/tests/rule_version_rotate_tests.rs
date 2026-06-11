//! W16 P1（docs/plan/16-config-versioning.md §4/§7）：`rotate_active_config`
//! 的 sha-keyed 轮换语义——首次 seed / 同 sha no-op / 漂移轮换 / 回滚复活 /
//! placeholder 收编 / pending 行 promote。

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rss_ai_news_storage::{
    ConfigRotation, RuleVersionRepo, RuleVersionRepository, StoragePool, build_sqlite_pool,
    run_migrations,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

static TEST_DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

async fn make_test_pool() -> (PathBuf, SqlitePool) {
    let counter = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rss-ai-news-rule-rotate-{}-{nanos}-{counter}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir should be created");
    let db_path = dir.join("test.sqlite");
    let pool = build_sqlite_pool(&db_path, 1, 5_000)
        .await
        .expect("test pool should be created");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrations should apply");
    (dir, pool)
}

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

async fn rotate(repo: &RuleVersionRepo, sha: &str) -> ConfigRotation {
    repo.rotate_active_config(sha, "test config", OffsetDateTime::now_utc())
        .await
        .expect("rotate should succeed")
}

/// (status, payload_sha256, version_tag, retired_at)
async fn rule_row(pool: &SqlitePool, id: i64) -> (String, String, String, Option<String>) {
    sqlx::query_as(
        "SELECT status, payload_sha256, version_tag, retired_at FROM rule_versions WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("rule row should exist")
}

async fn count_active_config(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM rule_versions WHERE kind='config' AND status='active'")
        .fetch_one(pool)
        .await
        .expect("count should succeed")
}

#[tokio::test]
async fn rotate_on_empty_db_seeds_active_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool.clone());

    let outcome = rotate(&repo, SHA_A).await;

    let ConfigRotation::Rotated { new_id, demoted_id } = outcome else {
        panic!("first rotate on empty db should report Rotated, got {outcome:?}");
    };
    assert_eq!(demoted_id, None, "empty db has no active row to demote");
    let (status, payload, tag, retired_at) = rule_row(&pool, new_id).await;
    assert_eq!(status, "active");
    assert_eq!(payload, SHA_A);
    assert_eq!(tag, &SHA_A[..12], "version_tag should be sha prefix");
    assert_eq!(retired_at, None);
    assert_eq!(count_active_config(&pool).await, 1);
}

#[tokio::test]
async fn rotate_same_sha_is_noop() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool.clone());

    let first = rotate(&repo, SHA_A).await;
    let second = rotate(&repo, SHA_A).await;

    assert_eq!(
        second,
        ConfigRotation::NoChange {
            id: first.active_id()
        },
        "same sha should be a zero-write no-op returning the same row"
    );
    assert_eq!(count_active_config(&pool).await, 1);
}

#[tokio::test]
async fn rotate_drift_demotes_old_active_and_inserts_new() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool.clone());

    let a_id = rotate(&repo, SHA_A).await.active_id();
    let outcome = rotate(&repo, SHA_B).await;

    let ConfigRotation::Rotated { new_id, demoted_id } = outcome else {
        panic!("sha drift should rotate, got {outcome:?}");
    };
    assert_eq!(demoted_id, Some(a_id));
    assert_ne!(new_id, a_id);
    let (a_status, _, _, a_retired) = rule_row(&pool, a_id).await;
    assert_eq!(a_status, "superseded");
    assert!(a_retired.is_some(), "demoted row should carry retired_at");
    let (b_status, b_payload, _, _) = rule_row(&pool, new_id).await;
    assert_eq!(b_status, "active");
    assert_eq!(b_payload, SHA_B);
    assert_eq!(count_active_config(&pool).await, 1);
}

#[tokio::test]
async fn rotate_rollback_revives_original_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool.clone());

    let a_id = rotate(&repo, SHA_A).await.active_id();
    let b_id = rotate(&repo, SHA_B).await.active_id();
    let outcome = rotate(&repo, SHA_A).await;

    assert_eq!(
        outcome,
        ConfigRotation::Rotated {
            new_id: a_id,
            demoted_id: Some(b_id),
        },
        "rollback A→B→A must revive the original A row, not insert a duplicate"
    );
    let (a_status, _, _, a_retired) = rule_row(&pool, a_id).await;
    assert_eq!(a_status, "active");
    assert_eq!(a_retired, None, "revival should clear retired_at");
    let (b_status, _, _, _) = rule_row(&pool, b_id).await;
    assert_eq!(b_status, "superseded");
    assert_eq!(count_active_config(&pool).await, 1);
}

#[tokio::test]
async fn rotate_supersedes_bootstrap_placeholder() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool.clone());

    // 模拟 D1 存量库：ingest bootstrap placeholder 是首个 active config 行。
    let placeholder_id = repo
        .active_rule_or_register(
            "config",
            "ingest-bootstrap",
            "auto-registered by ingest when no active config rule existed",
            "ingest-bootstrap",
        )
        .await
        .expect("placeholder seed should succeed");

    let outcome = rotate(&repo, SHA_A).await;

    let ConfigRotation::Rotated { new_id, demoted_id } = outcome else {
        panic!("placeholder active should be rotated out, got {outcome:?}");
    };
    assert_eq!(demoted_id, Some(placeholder_id));
    let (ph_status, _, _, _) = rule_row(&pool, placeholder_id).await;
    assert_eq!(ph_status, "superseded", "placeholder self-heals on rotate");
    let (status, payload, _, _) = rule_row(&pool, new_id).await;
    assert_eq!(status, "active");
    assert_eq!(payload, SHA_A);
}

#[tokio::test]
async fn rotate_promotes_existing_pending_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RuleVersionRepo::new(pool.clone());

    let a_id = rotate(&repo, SHA_A).await.active_id();
    // 既有 active 时 get_or_create 的 CASE/EXISTS 把新行写成 pending。
    let pending_id = repo
        .get_or_create("config", &SHA_C[..12], "pending config row", SHA_C)
        .await
        .expect("pending insert should succeed");
    let (status, _, _, _) = rule_row(&pool, pending_id).await;
    assert_eq!(status, "pending", "precondition: row starts pending");

    let outcome = rotate(&repo, SHA_C).await;

    assert_eq!(
        outcome,
        ConfigRotation::Rotated {
            new_id: pending_id,
            demoted_id: Some(a_id),
        },
        "rotate must reuse the existing row for the same sha"
    );
    let (status, _, _, retired_at) = rule_row(&pool, pending_id).await;
    assert_eq!(status, "active");
    assert_eq!(retired_at, None);
    assert_eq!(count_active_config(&pool).await, 1);
}
