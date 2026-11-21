//! B1 / migration 0008: existing publish_records migrate safely to the
//! per-stage budget. Rows written under 0007 get `stage_attempt_count = 0`
//! (the current stage starts with a fresh budget), terminal rows stay
//! terminal, cumulative `attempt_count` is untouched, and the down migration
//! restores the 0007 shape without losing rows.

mod common;

use std::borrow::Cow;

use rss_ai_news_storage::{
    ClaimRequest, PublishRecordRepo, PublishRecordRepository, StoragePool, lease_expires_at,
    run_migrations,
};
use sqlx::migrate::Migrator;
use time::{Duration, OffsetDateTime};

fn up_to_0007(mut migrator: Migrator) -> Migrator {
    migrator.migrations = Cow::Owned(
        migrator
            .iter()
            .filter(|migration| migration.version <= 7)
            .cloned()
            .collect(),
    );
    migrator
}

const SEED_0007_ROWS: &str = "
INSERT INTO rule_versions (id, kind, version_tag, description, payload_sha256, status)
VALUES (9001, 'render', 'mig-render', 'r', 'sha-r', 'superseded'),
       (9002, 'selection_policy', 'mig-policy', 'p', 'sha-p', 'superseded');
INSERT INTO publish_records (id, idempotency_key, category_key, report_date, target_timezone,
    render_version, selection_policy_version, state, attempt_count, remote_target)
VALUES (9101, 'mig-failed', 'ai', '2026-11-20', 'UTC', 9001, 9002, 'failed', 5, 'o/r:main'),
       (9102, 'mig-stored', 'ml', '2026-11-20', 'UTC', 9001, 9002, 'stored_local', 4, 'o/r:main');
";

type Row = (i64, String, i64, i64);
const ROWS: &str =
    "SELECT id, state, attempt_count, stage_attempt_count FROM publish_records ORDER BY id";

fn claim(now: OffsetDateTime) -> ClaimRequest {
    ClaimRequest {
        owner: "after-migration".to_string(),
        now,
        lease_expires_at: lease_expires_at(now, Duration::seconds(30)),
        batch_size: 10,
        max_attempts: 5,
    }
}

#[tokio::test]
async fn sqlite_0008_gives_existing_rows_a_fresh_stage_budget_and_rolls_back() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    up_to_0007(sqlx::migrate!("../../migrations/sqlite"))
        .run(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(SEED_0007_ROWS).execute(&pool).await.unwrap();

    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .unwrap();

    let rows: Vec<Row> = sqlx::query_as(ROWS).fetch_all(&pool).await.unwrap();
    assert_eq!(
        rows,
        vec![
            (9101, "failed".to_string(), 5, 0),
            (9102, "stored_local".to_string(), 4, 0),
        ]
    );
    // The stored_local row (4 cumulative attempts, max 5) is claimable once
    // more under the old rules too; under the stage budget it has 5 left.
    let claimed = PublishRecordRepo::new(pool.clone())
        .claim_local_for_remote_publish(&claim(OffsetDateTime::now_utc()))
        .await
        .unwrap();
    assert_eq!(claimed.iter().map(|r| r.id).collect::<Vec<_>>(), vec![9102]);

    sqlx::migrate!("../../migrations/sqlite")
        .undo(&pool, 7)
        .await
        .unwrap();
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('publish_records')")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(!columns.iter().any(|name| name == "stage_attempt_count"));
    let kept: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT id, state, attempt_count FROM publish_records ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        kept,
        vec![
            (9101, "failed".to_string(), 5),
            (9102, "stored_local".to_string(), 5),
        ]
    );
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_0008_gives_existing_rows_a_fresh_stage_budget_and_rolls_back() {
    let ctx = common::pg::make_pg_test_pool().await;
    let migrator = sqlx::migrate!("../../migrations/postgres");
    migrator.undo(ctx.pg_pool(), 7).await.unwrap();
    sqlx::raw_sql(SEED_0007_ROWS)
        .execute(ctx.pg_pool())
        .await
        .unwrap();

    run_migrations(ctx.storage_pool()).await.unwrap();

    let rows: Vec<Row> = sqlx::query_as(ROWS).fetch_all(ctx.pg_pool()).await.unwrap();
    assert_eq!(
        rows,
        vec![
            (9101, "failed".to_string(), 5, 0),
            (9102, "stored_local".to_string(), 4, 0),
        ]
    );
    let claimed = PublishRecordRepo::new_with_storage(ctx.storage_pool().clone())
        .claim_local_for_remote_publish(&claim(OffsetDateTime::now_utc()))
        .await
        .unwrap();
    assert_eq!(claimed.iter().map(|r| r.id).collect::<Vec<_>>(), vec![9102]);

    migrator.undo(ctx.pg_pool(), 7).await.unwrap();
    let has_column: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'publish_records' \
         AND column_name = 'stage_attempt_count')",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    assert!(!has_column);
    ctx.cleanup().await;
}
