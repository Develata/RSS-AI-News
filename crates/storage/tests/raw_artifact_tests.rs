mod common;

use rss_ai_news_domain::state::ArtifactKind;
use rss_ai_news_storage::{NewRawArtifact, RawArtifactRepo, RawArtifactRepository};

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
