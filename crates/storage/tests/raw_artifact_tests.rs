mod common;

use rss_ai_news_domain::state::ArtifactKind;
use rss_ai_news_storage::{NewRawArtifact, RawArtifactRepo, RawArtifactRepository, StoragePool};
use std::num::NonZeroU32;
use time::{Duration, OffsetDateTime};

use common::make_test_pool;

#[tokio::test]
async fn upsert_inline_inserts_new_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RawArtifactRepo::new(pool);

    let id = repo
        .upsert_inline(&artifact("feed_payload", "1", b"hello", "sha-a"))
        .await
        .expect("artifact should insert");

    assert!(id > 0);
}

#[tokio::test]
async fn upsert_inline_overwrites_existing_by_kind_and_key() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RawArtifactRepo::new(pool);

    let first = repo
        .upsert_inline(&artifact("feed_payload", "1", b"old", "sha-old"))
        .await
        .expect("first insert should succeed");
    let second = repo
        .upsert_inline(&artifact("feed_payload", "1", b"new", "sha-new"))
        .await
        .expect("second upsert should succeed");
    let found = repo
        .find_by_key("feed_payload", "1")
        .await
        .expect("find should succeed")
        .expect("artifact should exist");

    assert_eq!(first, second);
    assert_eq!(found.inline_body.as_deref(), Some(b"new".as_slice()));
    assert_eq!(found.sha256, "sha-new");
}

#[tokio::test]
async fn find_by_key_returns_none_when_missing() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RawArtifactRepo::new(pool);

    let found = repo
        .find_by_key("feed_payload", "missing")
        .await
        .expect("find should succeed");

    assert!(found.is_none());
}

#[tokio::test]
async fn find_by_key_returns_inserted_row() {
    let (_dir, pool) = make_test_pool().await;
    let repo = RawArtifactRepo::new(pool);

    let id = repo
        .upsert_inline(&artifact("feed_payload", "1", b"body", "sha-body"))
        .await
        .expect("artifact should insert");
    let found = repo
        .find_by_key("feed_payload", "1")
        .await
        .expect("find should succeed")
        .expect("artifact should exist");

    assert_eq!(found.id, id);
    assert_eq!(found.kind, ArtifactKind::FeedPayload);
    assert_eq!(found.artifact_key, "1");
    assert_eq!(found.storage_kind, "inline");
    assert_eq!(found.byte_size, 4);
}

fn artifact(kind: &str, key: &str, body: &[u8], sha256: &str) -> NewRawArtifact {
    NewRawArtifact {
        kind: kind.to_string(),
        artifact_key: key.to_string(),
        content_encoding: "utf-8".to_string(),
        inline_body: body.to_vec(),
        byte_size: body.len() as i64,
        sha256: sha256.to_string(),
        retention_policy: "always".to_string(),
        expires_at: None,
    }
}

async fn purge_contract(pool: StoragePool) {
    let repo = RawArtifactRepo::new_with_storage(pool.clone());
    let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let mut expired_ids = Vec::new();
    for i in 0..4 {
        let mut row = artifact("feed_payload", &format!("expired-{i}"), b"payload", "hash");
        row.expires_at = Some(now - Duration::days(4 - i));
        expired_ids.push(repo.upsert_inline(&row).await.unwrap());
    }
    let mut retained_ids = Vec::new();
    for (key, expiry) in [
        ("forever", None),
        ("at-cutoff", Some(now)),
        (
            "same-second-future",
            Some(now + Duration::milliseconds(500)),
        ),
        ("future", Some(now + Duration::days(1))),
    ] {
        let mut row = artifact("feed_payload", key, b"keep", "hash");
        row.expires_at = expiry;
        retained_ids.push(repo.upsert_inline(&row).await.unwrap());
    }
    let mut file = artifact("html_payload", "file", b"file", "hash");
    file.expires_at = Some(now - Duration::days(9));
    let file_id = repo.upsert_inline(&file).await.unwrap();
    let sql = "UPDATE raw_artifacts SET storage_kind='file', inline_body=NULL, file_path='untouched-fixture' WHERE id=$1";
    match &pool {
        StoragePool::Sqlite(p) => sqlx::query(sql)
            .bind(file_id)
            .execute(p)
            .await
            .unwrap()
            .rows_affected(),
        StoragePool::Postgres(p) => sqlx::query(sql)
            .bind(file_id)
            .execute(p)
            .await
            .unwrap()
            .rows_affected(),
    };
    retained_ids.push(file_id);
    assert_eq!(
        repo.purge_expired(now, NonZeroU32::new(2).unwrap())
            .await
            .unwrap(),
        2
    );
    for id in &expired_ids[..2] {
        assert!(repo.find_by_id(*id).await.unwrap().is_none());
    }
    for id in &expired_ids[2..] {
        assert!(repo.find_by_id(*id).await.unwrap().is_some());
    }
    assert_eq!(
        repo.purge_expired(now, NonZeroU32::new(10).unwrap())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        repo.purge_expired(now, NonZeroU32::new(10).unwrap())
            .await
            .unwrap(),
        0
    );
    for id in retained_ids {
        assert!(repo.find_by_id(id).await.unwrap().is_some());
    }

    // Renew an existing artifact before cleanup; its identity and payload survive.
    let mut renewed = artifact("feed_payload", "renewed", b"old", "hash");
    renewed.expires_at = Some(now - Duration::days(1));
    let id = repo.upsert_inline(&renewed).await.unwrap();
    renewed.expires_at = Some(now + Duration::days(1));
    renewed.inline_body = b"new".to_vec();
    assert_eq!(repo.upsert_inline(&renewed).await.unwrap(), id);
    assert_eq!(
        repo.purge_expired(now, NonZeroU32::new(10).unwrap())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        repo.find_by_id(id)
            .await
            .unwrap()
            .unwrap()
            .inline_body
            .unwrap(),
        b"new"
    );
}

#[tokio::test]
async fn sqlite_purge_expired_is_bounded_and_preserves_ineligible_rows() {
    let (_dir, pool) = make_test_pool().await;
    purge_contract(StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "requires Docker PostgreSQL fixture"]
async fn pg_purge_expired_is_bounded_and_preserves_ineligible_rows() {
    let ctx = common::pg::make_pg_test_pool().await;
    purge_contract(ctx.storage_pool().clone()).await;
    ctx.cleanup().await;
}

#[tokio::test]
async fn sqlite_purge_clears_only_artifact_references_and_uses_indexes() {
    let (_dir, pool) = make_test_pool().await;
    let (rule, _entry, article) = common::seed_article(&pool).await;
    let repo = RawArtifactRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();
    let mut row = artifact("html_payload", "referenced", b"raw", "hash");
    row.expires_at = Some(now - Duration::days(1));
    let id = repo.upsert_inline(&row).await.unwrap();
    sqlx::query("UPDATE articles SET body_html_artifact_id=$1 WHERE id=$2")
        .bind(id)
        .bind(article)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO article_ai_results (article_id,prompt_version,output_schema_version,model_id,state,summary,raw_response_artifact_id) VALUES ($1,$2,$2,'fixture','succeeded','kept summary',$3)")
        .bind(article).bind(rule).bind(id).execute(&pool).await.unwrap();
    assert_eq!(
        repo.purge_expired(now, NonZeroU32::new(1).unwrap())
            .await
            .unwrap(),
        1
    );
    let article_row: (String, Option<i64>) =
        sqlx::query_as("SELECT body_text,body_html_artifact_id FROM articles WHERE id=$1")
            .bind(article)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(article_row, ("body".into(), None));
    let result: (String, String, Option<i64>) = sqlx::query_as(
        "SELECT state,summary,raw_response_artifact_id FROM article_ai_results WHERE article_id=$1",
    )
    .bind(article)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(result, ("succeeded".into(), "kept summary".into(), None));
    let plans: Vec<(i64, i64, i64, String)> =
        sqlx::query_as("EXPLAIN QUERY PLAN DELETE FROM raw_artifacts WHERE id=-1")
            .fetch_all(&pool)
            .await
            .unwrap();
    for index in [
        "idx_articles_body_html_artifact_id",
        "idx_article_ai_results_raw_response_artifact_id",
    ] {
        assert!(
            plans.iter().any(|row| row.3.contains(index)),
            "missing {index}: {plans:?}"
        );
    }
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn sqlite_concurrent_purgers_delete_disjoint_bounded_batches() {
    let (_dir, pool) = common::make_test_pool_with_connections(4).await;
    let repo = RawArtifactRepo::new(pool.clone());
    let now = OffsetDateTime::now_utc();
    for i in 0..10 {
        let mut row = artifact("feed_payload", &i.to_string(), b"body", "hash");
        row.expires_at = Some(now - Duration::days(1));
        repo.upsert_inline(&row).await.unwrap();
    }
    let (a, b) = tokio::join!(
        repo.purge_expired(now, NonZeroU32::new(3).unwrap()),
        repo.purge_expired(now, NonZeroU32::new(3).unwrap())
    );
    assert_eq!((a.unwrap(), b.unwrap()), (3, 3));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM raw_artifacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 4);
}

#[tokio::test]
#[ignore = "requires Docker PostgreSQL fixture"]
async fn pg_purge_skips_concurrent_renewal_without_waiting_for_its_lock() {
    let ctx = common::pg::make_pg_test_pool().await;
    let repo = RawArtifactRepo::new_with_storage(ctx.storage_pool().clone());
    let now = OffsetDateTime::now_utc();
    let mut row = artifact("feed_payload", "locked", b"body", "hash");
    row.expires_at = Some(now - Duration::days(1));
    let id = repo.upsert_inline(&row).await.unwrap();
    let mut tx = ctx.pg_pool().begin().await.unwrap();
    sqlx::query("UPDATE raw_artifacts SET expires_at=$1 WHERE id=$2")
        .bind(now + Duration::days(1))
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let count = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        repo.purge_expired(now, NonZeroU32::new(500).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(count, 0);
    tx.commit().await.unwrap();
    assert!(repo.find_by_id(id).await.unwrap().is_some());
    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "requires Docker PostgreSQL fixture"]
async fn pg_purge_times_out_on_locked_reference_and_recovers_without_partial_deletion() {
    let ctx = common::pg::make_pg_test_pool().await;
    let pool = ctx.pg_pool();
    sqlx::raw_sql(include_str!("fixtures/v0.7.1/business.sql"))
        .execute(pool)
        .await
        .unwrap();
    let repo = RawArtifactRepo::new_with_storage(ctx.storage_pool().clone());
    let now = OffsetDateTime::now_utc();
    let mut row = artifact("html_payload", "locked-reference", b"body", "hash");
    row.expires_at = Some(now - Duration::days(1));
    let id = repo.upsert_inline(&row).await.unwrap();
    sqlx::query("UPDATE articles SET body_html_artifact_id=$1 WHERE id=100")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM articles WHERE id=100 FOR UPDATE")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        repo.purge_expired(now, NonZeroU32::new(500).unwrap()),
    )
    .await;
    // Always release the fixture lock, including on regression.
    lock.rollback().await.unwrap();
    let error = result
        .expect("cleanup must bound foreign-key lock waits")
        .unwrap_err();
    assert!(
        matches!(error, rss_ai_news_storage::StorageError::Sqlx(ref e)
        if e.as_database_error().and_then(|e| e.code()).as_deref() == Some("55P03"))
    );
    assert!(repo.find_by_id(id).await.unwrap().is_some());
    let reference: Option<i64> =
        sqlx::query_scalar("SELECT body_html_artifact_id FROM articles WHERE id=100")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(reference, Some(id));
    assert_eq!(
        repo.purge_expired(now, NonZeroU32::new(500).unwrap())
            .await
            .unwrap(),
        1
    );
    let reference: Option<i64> =
        sqlx::query_scalar("SELECT body_html_artifact_id FROM articles WHERE id=100")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(reference, None);
    ctx.cleanup().await;
}
