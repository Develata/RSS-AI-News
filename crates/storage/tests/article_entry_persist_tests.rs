//! A12: `ArticleRepository::persist_for_entry` commits the article and the
//! owner-guarded entry advance together (SQLite; PG mirror in
//! `article_pg_tests.rs`).

mod common;

use rss_ai_news_storage::{
    ArticleRepo, ArticleRepository, EntryPersistOutcome, NewArticle, StorageError,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{insert_feed_entry, insert_rule, make_test_pool, seed_source};

const OWNER: &str = "worker-a";

async fn claimed_entry(pool: &SqlitePool, source_id: i64, uid: &str) -> i64 {
    let id = insert_feed_entry(pool, source_id, uid, &format!("link-{uid}")).await;
    sqlx::query("UPDATE feed_entries SET state = 'fetching', lease_owner = ? WHERE id = ?")
        .bind(OWNER)
        .bind(id)
        .execute(pool)
        .await
        .expect("claim entry");
    id
}

fn article(hash: &str, entry_id: i64, extractor_version: i64) -> NewArticle {
    NewArticle {
        content_hash: hash.to_string(),
        canonical_link: "https://example.com/a".to_string(),
        title: "t".to_string(),
        body_text: "body".to_string(),
        body_html_artifact_id: None,
        extractor_strategy: "readability".to_string(),
        extractor_version,
        content_quality: "high".to_string(),
        word_count: 1,
        origin_feed_entry_id: entry_id,
    }
}

async fn entry(
    pool: &SqlitePool,
    id: i64,
) -> (String, Option<i64>, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT state, article_id, dedup_decision, lease_owner FROM feed_entries WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("entry row")
}

async fn article_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM articles")
        .fetch_one(pool)
        .await
        .unwrap()
}

struct Fixture {
    _dir: std::path::PathBuf,
    pool: SqlitePool,
    source_id: i64,
    extractor: i64,
    repo: ArticleRepo,
}

async fn fixture() -> Fixture {
    let (dir, pool) = make_test_pool().await;
    let source_id = seed_source(&pool).await;
    let extractor = insert_rule(&pool, "extractor", "ext", "ext-sha").await;
    let repo = ArticleRepo::new(pool.clone());
    Fixture {
        _dir: dir,
        pool,
        source_id,
        extractor,
        repo,
    }
}

#[tokio::test]
async fn new_article_and_entry_advance_commit_together() {
    let f = fixture().await;
    let id = claimed_entry(&f.pool, f.source_id, "new").await;

    let outcome = f
        .repo
        .persist_for_entry(
            &article("h-new", id, f.extractor),
            OWNER,
            false,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    let EntryPersistOutcome::Persisted {
        article_id,
        newly_created: true,
        fallback: false,
    } = outcome
    else {
        panic!("expected a new article, got {outcome:?}");
    };
    assert_eq!(
        entry(&f.pool, id).await,
        (
            "persisted".to_string(),
            Some(article_id),
            Some("fresh".to_string()),
            None
        )
    );
}

#[tokio::test]
async fn fallback_flag_selects_fallback_persisted() {
    let f = fixture().await;
    let id = claimed_entry(&f.pool, f.source_id, "fb").await;

    let outcome = f
        .repo
        .persist_for_entry(
            &article("h-fb", id, f.extractor),
            OWNER,
            true,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    assert!(matches!(outcome, EntryPersistOutcome::Persisted { .. }));
    assert_eq!(entry(&f.pool, id).await.0, "fallback_persisted");
}

#[tokio::test]
async fn lost_lease_rolls_back_the_article_insert() {
    let f = fixture().await;
    let id = claimed_entry(&f.pool, f.source_id, "lost").await;

    let outcome = f
        .repo
        .persist_for_entry(
            &article("h-lost", id, f.extractor),
            "someone-else",
            false,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, EntryPersistOutcome::LeaseLost);
    assert_eq!(article_count(&f.pool).await, 0, "no orphan article");
    assert_eq!(entry(&f.pool, id).await.0, "fetching");
}

#[tokio::test]
async fn own_article_from_an_earlier_attempt_counts_as_persisted_not_dedup() {
    let f = fixture().await;
    let id = claimed_entry(&f.pool, f.source_id, "self").await;
    // An earlier attempt committed the article, then crashed before the release.
    let first = f
        .repo
        .insert_or_get_by_content_hash(&article("h-self", id, f.extractor))
        .await
        .unwrap();

    let outcome = f
        .repo
        .persist_for_entry(
            &article("h-self", id, f.extractor),
            OWNER,
            false,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        EntryPersistOutcome::Persisted {
            article_id: first.article_id,
            newly_created: false,
            fallback: false,
        }
    );
    let (state, article_id, decision, _) = entry(&f.pool, id).await;
    assert_eq!(state, "persisted");
    assert_eq!(article_id, Some(first.article_id));
    assert_eq!(decision.as_deref(), Some("fresh"), "not marked hash_dup");
}

#[tokio::test]
async fn another_entrys_article_is_a_dedup() {
    let f = fixture().await;
    let owner_entry = claimed_entry(&f.pool, f.source_id, "first").await;
    let dup_entry = claimed_entry(&f.pool, f.source_id, "second").await;
    let EntryPersistOutcome::Persisted { article_id, .. } = f
        .repo
        .persist_for_entry(
            &article("h-dup", owner_entry, f.extractor),
            OWNER,
            false,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
    else {
        panic!("first entry persists");
    };

    let outcome = f
        .repo
        .persist_for_entry(
            &article("h-dup", dup_entry, f.extractor),
            OWNER,
            false,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, EntryPersistOutcome::DedupSkipped { article_id });
    assert_eq!(
        entry(&f.pool, dup_entry).await,
        (
            "dedup_skipped".to_string(),
            Some(article_id),
            Some("hash_dup".to_string()),
            None
        )
    );
    assert_eq!(article_count(&f.pool).await, 1);
}
// === R2-S6: recovery reuses the article this entry already owns ===

#[tokio::test]
async fn retry_with_a_changed_body_reuses_the_article_the_entry_owns() {
    let f = fixture().await;
    let id = claimed_entry(&f.pool, f.source_id, "s6-body").await;
    // A pre-upgrade attempt committed H1 then crashed before the release.
    let first = f
        .repo
        .insert_or_get_by_content_hash(&article("h-s6-1", id, f.extractor))
        .await
        .unwrap();

    // The retry extracted a different body (H2).
    let outcome = f
        .repo
        .persist_for_entry(
            &article("h-s6-2", id, f.extractor),
            OWNER,
            false,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        EntryPersistOutcome::Persisted {
            article_id: first.article_id,
            newly_created: false,
            fallback: false,
        }
    );
    assert_eq!(
        article_count(&f.pool).await,
        1,
        "no second article for the entry"
    );
    let (state, article_id, _, _) = entry(&f.pool, id).await;
    assert_eq!(state, "persisted");
    assert_eq!(article_id, Some(first.article_id));
}

#[tokio::test]
async fn recovery_takes_the_entry_state_from_the_stored_article() {
    let f = fixture().await;
    let id = claimed_entry(&f.pool, f.source_id, "s6-fallback").await;
    // The stored article came from the summary fallback ...
    let mut stored = article("h-s6-fb", id, f.extractor);
    stored.extractor_strategy = "summary_fallback".to_string();
    stored.content_quality = "fallback".to_string();
    f.repo.insert_or_get_by_content_hash(&stored).await.unwrap();

    // ... the retry got a full body.
    let outcome = f
        .repo
        .persist_for_entry(
            &article("h-s6-full", id, f.extractor),
            OWNER,
            false,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    assert_eq!(entry(&f.pool, id).await.0, "fallback_persisted");
    // R3E-S6: the outcome reports the stored state, not the call's flag.
    assert!(
        matches!(
            outcome,
            EntryPersistOutcome::Persisted { fallback: true, .. }
        ),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn an_entry_owning_several_articles_is_reported_not_guessed() {
    let f = fixture().await;
    let id = claimed_entry(&f.pool, f.source_id, "s6-many").await;
    for hash in ["h-s6-a", "h-s6-b"] {
        f.repo
            .insert_or_get_by_content_hash(&article(hash, id, f.extractor))
            .await
            .unwrap();
    }

    let error = f
        .repo
        .persist_for_entry(
            &article("h-s6-c", id, f.extractor),
            OWNER,
            false,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect_err("ambiguous ownership");

    assert!(matches!(error, StorageError::Integrity { .. }), "{error:?}");
    assert_eq!(article_count(&f.pool).await, 2, "nothing written");
    assert_eq!(entry(&f.pool, id).await.0, "fetching");
}
