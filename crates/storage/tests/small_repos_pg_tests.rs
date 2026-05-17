//! W11-P3-E-1：rule_version / raw_artifact / run_event 三个小型 repo PG 分支冒烟。
//!
//! 这三 repo 都没有 lease guard / 批量 claim 等复杂语义，SQL 跨方言完全等价，
//! 每 repo 1-2 个 happy 验证 PG 路径走通即可。
//!
//! 默认 `#[ignore]`，需要 docker。

mod common;

use common::pg::make_pg_test_pool;
use rss_ai_news_storage::{
    NewRawArtifact, NewRunEvent, RawArtifactRepo, RawArtifactRepository, RuleVersionRepo,
    RuleVersionRepository, RunEventRepo, RunEventRepository,
};
use time::OffsetDateTime;

// ── RuleVersionRepo ────────────────────────────────────────────

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_rule_version_get_or_create_then_active_rule() {
    let ctx = make_pg_test_pool().await;
    let repo = RuleVersionRepo::new_with_storage(ctx.storage_pool().clone());

    // 首版：CASE/EXISTS 看到该 kind 无 active 行 → 自动写 'active'
    let id_first = repo
        .get_or_create("config", "tag-1", "first", "sha-1")
        .await
        .expect("pg get_or_create first");
    assert!(id_first > 0);

    let active = repo
        .active_rule("config")
        .await
        .expect("pg active_rule")
        .expect("active row present");
    assert_eq!(active.id, id_first);
    assert_eq!(active.kind, "config");
    assert_eq!(active.version_tag, "tag-1");
    assert_eq!(active.status, "active");

    // 二次同 (kind, tag)：ON CONFLICT DO NOTHING → 兜底 SELECT 返同 id
    let id_again = repo
        .get_or_create("config", "tag-1", "first-again", "sha-1-again")
        .await
        .expect("pg get_or_create same key");
    assert_eq!(id_again, id_first, "ON CONFLICT returns existing id");

    // 新 (kind, tag)：已有 active 行 → CASE/EXISTS 让新行写 'pending'
    let id_second = repo
        .get_or_create("config", "tag-2", "second", "sha-2")
        .await
        .expect("pg get_or_create second tag");
    assert_ne!(id_second, id_first);

    // active_rule 仍返 tag-1（partial unique 保证 active 唯一）
    let still_active = repo.active_rule("config").await.unwrap().unwrap();
    assert_eq!(still_active.id, id_first);
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_rule_version_insert_pending_rule() {
    let ctx = make_pg_test_pool().await;
    let repo = RuleVersionRepo::new_with_storage(ctx.storage_pool().clone());

    let id = repo
        .insert_pending_rule("extractor", "v9", "pending-row", "sha-v9")
        .await
        .expect("pg insert_pending_rule");
    assert!(id > 0);

    // pending 行不影响 active_rule（该 kind 当前无 active）
    let active = repo.active_rule("extractor").await.unwrap();
    assert!(
        active.is_none(),
        "pending row must not be returned as active"
    );
}

// ── RawArtifactRepo ────────────────────────────────────────────

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_raw_artifact_upsert_inline_then_find_by_key() {
    let ctx = make_pg_test_pool().await;
    let repo = RawArtifactRepo::new_with_storage(ctx.storage_pool().clone());

    let payload = NewRawArtifact {
        kind: "feed_payload".to_string(),
        artifact_key: "https://example.com/feed.xml".to_string(),
        content_encoding: "utf-8".to_string(),
        inline_body: b"<rss>hello</rss>".to_vec(),
        byte_size: 16,
        sha256: "sha-feed".to_string(),
        retention_policy: "ephemeral".to_string(),
        expires_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
    };
    let id = repo
        .upsert_inline(&payload)
        .await
        .expect("pg upsert_inline first");
    assert!(id > 0);

    let found = repo
        .find_by_key("feed_payload", "https://example.com/feed.xml")
        .await
        .expect("pg find_by_key")
        .expect("inserted row present");
    assert_eq!(found.id, id);
    assert_eq!(found.byte_size, 16);
    assert_eq!(found.sha256, "sha-feed");
    assert_eq!(
        found.inline_body.as_deref(),
        Some(b"<rss>hello</rss>".as_slice()),
        "BYTEA roundtrip must preserve bytes"
    );

    // 二次 upsert 同 key：ON CONFLICT DO UPDATE 返同 id + 字段已更新
    let updated = NewRawArtifact {
        inline_body: b"<rss>updated</rss>".to_vec(),
        byte_size: 18,
        sha256: "sha-feed-v2".to_string(),
        ..payload
    };
    let id2 = repo.upsert_inline(&updated).await.unwrap();
    assert_eq!(id2, id);
    let after = repo
        .find_by_key("feed_payload", "https://example.com/feed.xml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.sha256, "sha-feed-v2");
    assert_eq!(after.byte_size, 18);
}

// ── RunEventRepo ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_run_event_insert_returns_id() {
    let ctx = make_pg_test_pool().await;
    let repo = RunEventRepo::new_with_storage(ctx.storage_pool().clone());

    let event = NewRunEvent {
        run_id: "run-pg-1".to_string(),
        trace_id: Some("trace-1".to_string()),
        stage: "ingest".to_string(),
        severity: "info".to_string(),
        event_kind: "started".to_string(),
        target_kind: Some("feed_source".to_string()),
        target_id: Some(42),
        message: "feed fetch started".to_string(),
        context_json: Some(r#"{"key":"value"}"#.to_string()),
    };
    let id = repo.insert(&event).await.expect("pg run_event insert");
    assert!(id > 0);

    // sanity：再 INSERT 一条 NULL trace_id / NULL target，确认 NULL bind 工作
    let event2 = NewRunEvent {
        run_id: "run-pg-2".to_string(),
        trace_id: None,
        stage: "publish".to_string(),
        severity: "warn".to_string(),
        event_kind: "retry".to_string(),
        target_kind: None,
        target_id: None,
        message: "retrying".to_string(),
        context_json: None,
    };
    let id2 = repo
        .insert(&event2)
        .await
        .expect("pg run_event insert with NULLs");
    assert!(id2 > id);
}
