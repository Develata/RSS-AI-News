//! W11-P3-E-2：[`FeedEntryRepo`] PG 分支冒烟。
//!
//! 覆盖（基于 P3-C 模板）：
//!   - `insert_if_new` ON CONFLICT DO NOTHING + 二次返 None
//!   - `exists_by_link_hash` CASE WHEN EXISTS decode i32（PG 路径）
//!   - `claim_pending_fetch` → `release_success` lease 推进
//!   - `claim_pending_fetch` 确定性 SKIP LOCKED 验证（参考 P3-C-fix1 H2 模板）
//!   - `reset_failed_in_window` COUNT + UPDATE 跨方言
//!
//! 默认 `#[ignore]`，需要 docker。

mod common;

use common::pg::{PgTestContext, make_pg_test_pool};
use rss_ai_news_storage::{
    ClaimRequest, FeedEntryRepo, FeedEntryRepository, NewFeedEntry, RecentFeedEntryFilter,
    ResetFailedFilter, StoragePool, ensure_migration_state_exact,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

fn lease_expires(now: OffsetDateTime) -> OffsetDateTime {
    now + time::Duration::minutes(5)
}

fn parse_timestamp(value: &str) -> OffsetDateTime {
    OffsetDateTime::parse(value, &Rfc3339).expect("valid RFC3339 test timestamp")
}

async fn seed_article(ctx: &PgTestContext, hash: &str, entry_id: i64) -> i64 {
    let ext_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', $1, 'e', $2, 'superseded') RETURNING id",
    )
    .bind(format!("ext-{hash}"))
    .bind(format!("sha-ext-{hash}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO articles (content_hash, canonical_link, title, body_text, \
            extractor_strategy, extractor_version, content_quality, origin_feed_entry_id, state) \
         VALUES ($1, 'https://example.com/a', 'title', 'body', 'readability', $2, 'high', $3, 'persisted') \
         RETURNING id",
    )
    .bind(hash)
    .bind(ext_id)
    .bind(entry_id)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap()
}

async fn seed_feed_source(ctx: &PgTestContext, tag: &str) -> i64 {
    let rule_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('config', $1, 'c', $2, 'superseded') RETURNING id",
    )
    .bind(format!("cfg-{tag}"))
    .bind(format!("sha-cfg-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
            feed_kind, config_version) \
         VALUES ('ai', $1, 'AI Main', 'https://example.com/feed.xml', 'rss', $2) \
         RETURNING id",
    )
    .bind(format!("src-{tag}"))
    .bind(rule_id)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap()
}

fn new_feed_entry(source_id: i64, uid: &str) -> NewFeedEntry {
    NewFeedEntry {
        source_id,
        feed_entry_uid: uid.to_string(),
        normalized_link: format!("https://example.com/{uid}"),
        link_hash: format!("hash-{uid}"),
        title_raw: format!("Title {uid}"),
        summary_raw: Some("Summary".to_string()),
        published_at: None,
        discovered_at: OffsetDateTime::now_utc(),
    }
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_insert_if_new_returns_none_on_duplicate() {
    let ctx = make_pg_test_pool().await;
    let source_id = seed_feed_source(&ctx, "ins").await;
    let repo = FeedEntryRepo::new_with_storage(ctx.storage_pool().clone());

    let entry = new_feed_entry(source_id, "uid-1");
    let first = repo
        .insert_if_new(&entry)
        .await
        .expect("pg insert first")
        .expect("first inserts");
    let second = repo.insert_if_new(&entry).await.expect("pg insert second");
    assert!(
        second.is_none(),
        "ON CONFLICT(source_id, feed_entry_uid) DO NOTHING returns None on 2nd"
    );

    let found = repo.find_by_id(first).await.unwrap().unwrap();
    assert_eq!(found.feed_entry_uid, "uid-1");
    assert_eq!(found.state, "pending_fetch");

    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_exists_by_link_hash_with_case_when_decode_i32() {
    let ctx = make_pg_test_pool().await;
    let source_id = seed_feed_source(&ctx, "exists").await;
    let repo = FeedEntryRepo::new_with_storage(ctx.storage_pool().clone());

    let not_found = repo
        .exists_by_link_hash("hash-missing")
        .await
        .expect("pg exists_by_link_hash");
    assert!(!not_found);

    repo.insert_if_new(&new_feed_entry(source_id, "uid-exists"))
        .await
        .unwrap();
    let found = repo.exists_by_link_hash("hash-uid-exists").await.unwrap();
    assert!(found, "CASE WHEN EXISTS decode i32 must work on PG");

    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_claim_pending_fetch_then_release_success() {
    let ctx = make_pg_test_pool().await;
    let source_id = seed_feed_source(&ctx, "claim").await;
    let repo = FeedEntryRepo::new_with_storage(ctx.storage_pool().clone());

    let entry_id = repo
        .insert_if_new(&new_feed_entry(source_id, "uid-claim"))
        .await
        .unwrap()
        .unwrap();

    let now = OffsetDateTime::now_utc();
    let claimed = repo
        .claim_pending_fetch(&ClaimRequest {
            owner: "worker-A".to_string(),
            now,
            lease_expires_at: lease_expires(now),
            batch_size: 4,
            max_attempts: 3,
        })
        .await
        .expect("pg claim_pending_fetch");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, entry_id);

    // seed 真实 article（PG FK 严格校验，必须 articles 表里有对应行）
    let article_id = seed_article(&ctx, "hash-claim", entry_id).await;

    // release_success：state pending_fetch → persisted（这里实际是 fetching →
    // persisted，因为 claim 推到 fetching；但 release_success 只校验 lease_owner
    // 不卡 state，所以 ingest 直接 persisted 是契约保留）
    let advanced = repo
        .release_success(entry_id, "worker-A", article_id, now)
        .await
        .expect("pg release_success");
    assert!(advanced);

    let after = repo.find_by_id(entry_id).await.unwrap().unwrap();
    assert_eq!(after.state, "persisted");
    assert_eq!(after.article_id, Some(article_id));
    assert_eq!(after.lease_owner, None);

    ctx.cleanup().await;
}

/// P3-C-fix1 H2 模板：tx_a `SELECT FOR UPDATE` 锁住一条 pending_fetch 不提交 →
/// 另一连接 claim_pending_fetch 必跳过被锁行 + 拿后续候选。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_claim_pending_fetch_skips_explicitly_locked_row() {
    let ctx = make_pg_test_pool().await;
    let source_id = seed_feed_source(&ctx, "skip-lock").await;
    let repo = FeedEntryRepo::new_with_storage(ctx.storage_pool().clone());

    // 两条 pending_fetch；ORDER BY discovered_at ASC 应优先 entry_a
    let earlier = OffsetDateTime::now_utc() - time::Duration::seconds(10);
    let later = OffsetDateTime::now_utc();
    let mut e_a = new_feed_entry(source_id, "uid-skip-a");
    e_a.discovered_at = earlier;
    let mut e_b = new_feed_entry(source_id, "uid-skip-b");
    e_b.discovered_at = later;
    let id_a = repo.insert_if_new(&e_a).await.unwrap().unwrap();
    let id_b = repo.insert_if_new(&e_b).await.unwrap().unwrap();
    // 校正 discovered_at（INSERT SQL 用了字段值，OK）
    sqlx::query("UPDATE feed_entries SET discovered_at = $1 WHERE id = $2")
        .bind(earlier)
        .bind(id_a)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    sqlx::query("UPDATE feed_entries SET discovered_at = $1 WHERE id = $2")
        .bind(later)
        .bind(id_b)
        .execute(ctx.pg_pool())
        .await
        .unwrap();

    // tx_a 锁 entry_a 不提交
    let mut tx_a = ctx.pg_pool().begin().await.expect("begin tx_a");
    let locked: i64 = sqlx::query_scalar("SELECT id FROM feed_entries WHERE id = $1 FOR UPDATE")
        .bind(id_a)
        .fetch_one(&mut *tx_a)
        .await
        .expect("tx_a FOR UPDATE on entry_a");
    assert_eq!(locked, id_a);

    // worker B 调 claim_pending_fetch(batch_size=1)：SKIP LOCKED 跳 entry_a 拿 entry_b
    let now = OffsetDateTime::now_utc();
    let claimed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        repo.claim_pending_fetch(&ClaimRequest {
            owner: "worker-B".to_string(),
            now,
            lease_expires_at: lease_expires(now),
            batch_size: 1,
            max_attempts: 3,
        }),
    )
    .await
    .expect("claim_pending_fetch must return within 5s (else SKIP LOCKED regressed)")
    .expect("claim call");
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].id, id_b,
        "SKIP LOCKED must skip locked entry_a and pick entry_b"
    );

    tx_a.rollback().await.expect("rollback tx_a");
    let entry_a_state: String = sqlx::query_scalar("SELECT state FROM feed_entries WHERE id = $1")
        .bind(id_a)
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(
        entry_a_state, "pending_fetch",
        "entry_a must remain pending_fetch"
    );

    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_recent_entries_matches_sqlite_contract() {
    let ctx = make_pg_test_pool().await;
    let p20 = seed_feed_source(&ctx, "recent-p20").await;
    let p10 = seed_feed_source(&ctx, "recent-p10").await;
    let paused = seed_feed_source(&ctx, "recent-paused").await;
    let other = seed_feed_source(&ctx, "recent-other").await;
    sqlx::query("UPDATE feed_sources SET priority = 20 WHERE id = $1")
        .bind(p20)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    sqlx::query("UPDATE feed_sources SET priority = 10 WHERE id = $1")
        .bind(p10)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    sqlx::query("UPDATE feed_sources SET status = 'paused' WHERE id = $1")
        .bind(paused)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    sqlx::query("UPDATE feed_sources SET category_key = 'other' WHERE id = $1")
        .bind(other)
        .execute(ctx.pg_pool())
        .await
        .unwrap();

    let repo = FeedEntryRepo::new_with_storage(ctx.storage_pool().clone());
    let exact_boundary = OffsetDateTime::from_unix_timestamp(2_000).unwrap();
    let offset_boundary = parse_timestamp("1970-01-01T01:33:20+01:00");
    assert_eq!(offset_boundary, exact_boundary);
    for (source, uid) in [
        (p20, "p20"),
        (p10, "p10"),
        (paused, "paused"),
        (other, "other"),
        (p10, "dedup"),
    ] {
        let mut entry = new_feed_entry(source, uid);
        entry.discovered_at = exact_boundary;
        let id = repo.insert_if_new(&entry).await.unwrap().unwrap();
        if uid == "dedup" {
            sqlx::query("UPDATE feed_entries SET state = 'dedup_skipped' WHERE id = $1")
                .bind(id)
                .execute(ctx.pg_pool())
                .await
                .unwrap();
        }
    }

    let mut fractional = new_feed_entry(p10, "fractional-offset");
    fractional.discovered_at = parse_timestamp("1970-01-01T01:33:20.500+01:00");
    repo.insert_if_new(&fractional).await.unwrap().unwrap();

    let mut older = new_feed_entry(p10, "older-offset");
    older.discovered_at = parse_timestamp("1970-01-01T01:33:19.999+01:00");
    repo.insert_if_new(&older).await.unwrap().unwrap();

    let rows = repo
        .list_recent(&RecentFeedEntryFilter {
            category_key: "ai".to_string(),
            discovered_after: offset_boundary,
            max_rows: 10,
        })
        .await
        .expect("PG recent projection");

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].title, "Title fractional-offset");
    assert_eq!(rows[0].discovered_at, fractional.discovered_at);
    assert_eq!(rows[1].source_key, "src-recent-p10");
    assert_eq!(rows[1].source_priority, 10);
    assert_eq!(rows[1].discovered_at, exact_boundary);
    assert_eq!(rows[2].source_key, "src-recent-p20");
    assert_eq!(rows[2].discovered_at, exact_boundary);
    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_recent_entries_read_only_pool_enforces_session_and_rejects_writes() {
    let ctx = make_pg_test_pool().await;
    let source_id = seed_feed_source(&ctx, "recent-read-only").await;
    let writer_repo = FeedEntryRepo::new_with_storage(ctx.storage_pool().clone());
    writer_repo
        .insert_if_new(&new_feed_entry(source_id, "read-only-row"))
        .await
        .expect("seed through writer pool")
        .expect("new row");
    let priority_before: i64 =
        sqlx::query_scalar("SELECT priority FROM feed_sources WHERE id = $1")
            .bind(source_id)
            .fetch_one(ctx.pg_pool())
            .await
            .expect("priority before rejected write");

    let read_only = ctx.read_only_storage_pool().await;
    ensure_migration_state_exact(&read_only)
        .await
        .expect("read-only pool can inspect exact migration state");
    let read_only_pg = match &read_only {
        StoragePool::Postgres(pool) => pool,
        StoragePool::Sqlite(_) => panic!("PG fixture returned SQLite read-only pool"),
    };
    let setting: String = sqlx::query_scalar("SHOW default_transaction_read_only")
        .fetch_one(read_only_pg)
        .await
        .expect("read session default_transaction_read_only");
    assert_eq!(setting, "on");

    let reader_repo = FeedEntryRepo::new_with_storage(read_only.clone());
    let rows = reader_repo
        .list_recent(&RecentFeedEntryFilter {
            category_key: "ai".to_string(),
            discovered_after: OffsetDateTime::UNIX_EPOCH,
            max_rows: 10,
        })
        .await
        .expect("projection SELECT through PG read-only pool");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Title read-only-row");

    let error = sqlx::query("UPDATE feed_sources SET priority = priority + 1 WHERE id = $1")
        .bind(source_id)
        .execute(read_only_pg)
        .await
        .expect_err("read-only command pool must reject UPDATE");
    let sqlstate = error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .map(|code| code.into_owned());
    assert_eq!(sqlstate.as_deref(), Some("25006"));

    let priority_after: i64 = sqlx::query_scalar("SELECT priority FROM feed_sources WHERE id = $1")
        .bind(source_id)
        .fetch_one(ctx.pg_pool())
        .await
        .expect("priority after rejected write");
    assert_eq!(priority_after, priority_before);

    read_only_pg.close().await;
    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_reset_failed_in_window_counts_and_resets() {
    let ctx = make_pg_test_pool().await;
    let source_id = seed_feed_source(&ctx, "reset").await;
    let repo = FeedEntryRepo::new_with_storage(ctx.storage_pool().clone());

    // 写 2 条 entries，一条 failed 一条 persisted
    let id_failed = repo
        .insert_if_new(&new_feed_entry(source_id, "uid-failed"))
        .await
        .unwrap()
        .unwrap();
    let id_persisted = repo
        .insert_if_new(&new_feed_entry(source_id, "uid-persisted"))
        .await
        .unwrap()
        .unwrap();

    sqlx::query("UPDATE feed_entries SET state = 'failed', attempt_count = 5 WHERE id = $1")
        .bind(id_failed)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    sqlx::query("UPDATE feed_entries SET state = 'persisted' WHERE id = $1")
        .bind(id_persisted)
        .execute(ctx.pg_pool())
        .await
        .unwrap();

    let outcome = repo
        .reset_failed_in_window(&ResetFailedFilter::default())
        .await
        .expect("pg reset_failed_in_window");
    assert_eq!(outcome.examined, 2, "COUNT(*) sees both rows");
    assert_eq!(outcome.reset, 1, "only the failed row is reset");

    let reset_row = repo.find_by_id(id_failed).await.unwrap().unwrap();
    assert_eq!(
        reset_row.state, "pending_fetch",
        "failed → pending_fetch（claim 认的状态，非死状态 discovered）"
    );
    assert_eq!(reset_row.attempt_count, 0);

    ctx.cleanup().await;
}
