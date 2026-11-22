//! R2-S2 — C9 fresh ordering on SQLite: by source **instant** (not its text),
//! ties by **article** id (Incident 2), and the claim hands rows to the flow
//! in that order (an `UPDATE … RETURNING` returns rows in an unspecified
//! order). R2-S1's inward-rounded millisecond bounds are locked here too.

mod common;

use rss_ai_news_storage::{
    ArticleAiResultRepo, ArticleAiResultRepository, ArticleRepo, ArticleRepository, ClaimRequest,
    FreshWindow, NewAiResult,
};
use sqlx::SqlitePool;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use common::{insert_article, insert_feed_entry, insert_rule, make_test_pool, seed_source};

struct Fixture {
    pool: SqlitePool,
    rule: i64,
    source: i64,
    prompt: i64,
    schema: i64,
}

impl Fixture {
    async fn new() -> (std::path::PathBuf, Self) {
        let (dir, pool) = make_test_pool().await;
        let rule = insert_rule(&pool, "extractor", "x-order", "sha-x-order").await;
        let prompt = insert_rule(&pool, "prompt", "p-order", "sha-p-order").await;
        let schema = insert_rule(&pool, "ai_output_schema", "s-order", "sha-s-order").await;
        let source = seed_source(&pool).await;
        let fixture = Self {
            pool,
            rule,
            source,
            prompt,
            schema,
        };
        (dir, fixture)
    }

    /// A `persisted` article whose `feed_entries.published_at` is the raw
    /// TEXT `published_at`.
    async fn article(&self, tag: &str, published_at: &str) -> i64 {
        let entry = insert_feed_entry(
            &self.pool,
            self.source,
            &format!("u-{tag}"),
            &format!("l-{tag}"),
        )
        .await;
        sqlx::query("UPDATE feed_entries SET published_at = ? WHERE id = ?")
            .bind(published_at)
            .bind(entry)
            .execute(&self.pool)
            .await
            .unwrap();
        insert_article(&self.pool, &format!("c-{tag}"), entry, self.rule)
            .await
            .unwrap()
    }

    fn identity(&self) -> NewAiResult {
        NewAiResult {
            article_id: 0,
            prompt_version: self.prompt,
            output_schema_version: self.schema,
            model_id: "m".to_string(),
        }
    }

    async fn task(&self, article_id: i64) -> i64 {
        ArticleAiResultRepo::new(self.pool.clone())
            .insert_pending(&NewAiResult {
                article_id,
                ..self.identity()
            })
            .await
            .unwrap()
            .unwrap()
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc().replace_nanosecond(0).unwrap()
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap()
}

fn window(now: OffsetDateTime) -> FreshWindow {
    FreshWindow {
        since: now - Duration::hours(48),
        until: now,
    }
}

fn request(now: OffsetDateTime, batch_size: u32) -> ClaimRequest {
    ClaimRequest {
        owner: "w".to_string(),
        now,
        lease_expires_at: now + Duration::minutes(5),
        batch_size,
        max_attempts: 3,
    }
}

#[tokio::test]
async fn equal_instants_in_different_offsets_tie_by_article_id() {
    let (_dir, f) = Fixture::new().await;
    let now = now();
    let at = now - Duration::hours(1);
    // Same instant; the `+02:00` text sorts after the `Z` text.
    let low = f
        .article(
            "tie-low",
            &rfc3339(at.to_offset(UtcOffset::from_hms(2, 0, 0).unwrap())),
        )
        .await;
    let high = f.article("tie-high", &rfc3339(at)).await;

    let listed = ArticleRepo::new(f.pool.clone())
        .list_fresh_for_ai_task_gen("ai", window(now), 10, 0)
        .await
        .unwrap();

    assert_eq!(
        listed.iter().map(|c| c.article_id).collect::<Vec<_>>(),
        [low, high]
    );
}

#[tokio::test]
async fn fresh_claim_ties_by_article_id_not_task_id() {
    let (_dir, f) = Fixture::new().await;
    let now = now();
    let at = rfc3339(now - Duration::hours(1));
    let low = f.article("c-low", &at).await;
    let high = f.article("c-high", &at).await;
    // Tasks created in reverse: the higher article owns the lower task id.
    f.task(high).await;
    let low_task = f.task(low).await;

    let claimed = ArticleAiResultRepo::new(f.pool.clone())
        .claim_pending_shared(&request(now, 1), "ai", window(now).since)
        .await
        .unwrap();

    assert_eq!(claimed.iter().map(|c| c.id).collect::<Vec<_>>(), [low_task]);
}

#[tokio::test]
async fn claimed_rows_come_back_in_candidate_order() {
    let (_dir, f) = Fixture::new().await;
    let now = now();
    // Ascending ids, descending source times: oldest-first is #3, #2, #1.
    let mut articles = Vec::new();
    for hours in 1..=3 {
        let at = rfc3339(now - Duration::hours(hours));
        articles.push(f.article(&format!("o-{hours}"), &at).await);
    }
    for id in &articles {
        f.task(*id).await;
    }

    let claimed = ArticleAiResultRepo::new(f.pool.clone())
        .claim_pending_shared(&request(now, 3), "ai", window(now).since)
        .await
        .unwrap();

    assert_eq!(
        claimed.iter().map(|c| c.article_id).collect::<Vec<_>>(),
        [articles[2], articles[1], articles[0]]
    );
}

#[tokio::test]
async fn a_sub_millisecond_instant_before_the_window_start_is_not_fresh() {
    let (_dir, f) = Fixture::new().await;
    let now = now();
    let window = FreshWindow {
        since: now - Duration::hours(1) + Duration::microseconds(800),
        until: now,
    };
    // 0.1 ms into the window's first millisecond, 0.7 ms before `since`.
    let before = f
        .article(
            "ms-before",
            &rfc3339(now - Duration::hours(1) + Duration::microseconds(100)),
        )
        .await;
    let inside = f
        .article(
            "ms-inside",
            &rfc3339(now - Duration::hours(1) + Duration::milliseconds(2)),
        )
        .await;

    let listed = ArticleRepo::new(f.pool.clone())
        .list_fresh_for_ai_task_gen("ai", window, 10, 0)
        .await
        .unwrap();

    let ids = listed.iter().map(|c| c.article_id).collect::<Vec<_>>();
    assert_eq!(ids, [inside]);
    assert!(!ids.contains(&before));
}

/// R3E-B1 / R4-3: the two task_gen lists partition `persisted` on SQLite.
/// The fresh list holds every in-window article, inserts and C3
/// reconciliations alike, in one order (source instant, then article id);
/// the backlog list keeps only out-of-window articles (id order + cursor).
#[tokio::test]
async fn task_gen_lists_partition_persisted_by_window() {
    let (_dir, f) = Fixture::new().await;
    let now = now();
    let pending = f
        .article("p-pending", &rfc3339(now - Duration::hours(2)))
        .await;
    let fresh = f
        .article("p-fresh", &rfc3339(now - Duration::hours(1)))
        .await;
    let old = f.article("p-old", &rfc3339(now - Duration::days(10))).await;
    let old_with_task = f
        .article("p-old-task", &rfc3339(now - Duration::days(9)))
        .await;
    let failed = f
        .article("p-failed", &rfc3339(now - Duration::hours(3)))
        .await;
    for id in [pending, old_with_task, failed] {
        f.task(id).await;
    }
    let repo = ArticleRepo::new(f.pool.clone());
    let ids = |rows: Vec<rss_ai_news_storage::ArticleAiTaskCandidate>| {
        rows.iter()
            .map(|c| (c.article_id, c.in_window))
            .collect::<Vec<_>>()
    };

    let fresh_list = repo
        .list_fresh_for_ai_task_gen("ai", window(now), 10, 0)
        .await
        .unwrap();
    let after_cursor = repo
        .list_fresh_for_ai_task_gen("ai", window(now), 10, failed)
        .await
        .unwrap();
    let backlog = repo
        .list_backlog_for_ai_task_gen("ai", &f.identity(), window(now), 10, 0)
        .await
        .unwrap();

    assert_eq!(
        ids(fresh_list),
        [(failed, true), (pending, true), (fresh, true)]
    );
    assert_eq!(ids(after_cursor), [(pending, true), (fresh, true)]);
    assert_eq!(ids(backlog), [(old, false), (old_with_task, false)]);
}

/// R4-3: paging the fresh list one row at a time with the keyset cursor
/// visits every in-window article once, in (source instant, article id)
/// order, although ids ascend while source times descend and three rows
/// share one instant (one of them written in another offset).
#[tokio::test]
async fn fresh_list_cursor_walks_source_order_without_skip_or_repeat() {
    let (_dir, f) = Fixture::new().await;
    let now = now();
    let tie = now - Duration::hours(2);
    let early = f
        .article("w-early", &rfc3339(now - Duration::hours(3)))
        .await;
    let tie_low = f.article("w-tie-low", &rfc3339(tie)).await;
    let tie_mid = f
        .article(
            "w-tie-mid",
            &rfc3339(tie.to_offset(UtcOffset::from_hms(2, 0, 0).unwrap())),
        )
        .await;
    let tie_high = f.article("w-tie-high", &rfc3339(tie)).await;
    // Created after the ties but older than all of them.
    let earliest = f
        .article("w-earliest", &rfc3339(now - Duration::hours(4)))
        .await;
    let late = f
        .article("w-late", &rfc3339(now - Duration::hours(1)))
        .await;
    // Mixed kinds: some already have a task of the identity.
    for id in [tie_mid, earliest] {
        f.task(id).await;
    }
    let repo = ArticleRepo::new(f.pool.clone());

    let mut walked = Vec::new();
    let mut cursor = 0;
    loop {
        let page = repo
            .list_fresh_for_ai_task_gen("ai", window(now), 1, cursor)
            .await
            .unwrap();
        let Some(row) = page.first() else { break };
        cursor = row.article_id;
        walked.push(row.article_id);
        assert!(walked.len() <= 6, "the walk must end: {walked:?}");
    }

    assert_eq!(walked, [earliest, early, tie_low, tie_mid, tie_high, late]);
}
