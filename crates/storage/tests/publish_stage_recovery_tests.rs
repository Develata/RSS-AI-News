//! Publish 阶段态崩溃恢复 + 耗尽收口不变量。
//!
//! Publish 是阶段驱动状态机：每阶段各自 claim 自己的输入态（pending /
//! snapshot_frozen / rendered / stored_local），advance 成功即清 lease。故进程
//! 在阶段之间崩溃，会把 record 留在某个中间态、lease 已清。本文件锁两条恢复
//! 不变量，防止"claim 只认 pending、一趟 advance 到底"这类改动悄悄引回死状态：
//!
//!   1. 中间态 + 清 lease + 预算未耗尽 → 只能被**对应阶段**的 claim 续跑（不串台），
//!      claim 后 attempt_count 自增。保证崩溃后没有"无人认领"的非终态。
//!   2. 中间态 + 过期 lease + 预算耗尽 → reclaim 清 lease 后，terminalize sweep
//!      把四个阶段态全部收口到 failed（既不无限重试，也不卡死）。

mod common;

use rss_ai_news_storage::{
    ClaimRequest, NewPublishRecord, PublishRecordRepo, PublishRecordRepository, build_owner_id,
    lease_expires_at,
};
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use common::{insert_rule, make_test_pool};

const MAX_ATTEMPTS: u32 = 5;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp")
}

/// 建一条 publish_record 并强制其 state / attempt_count / lease 字段，
/// 模拟崩溃后留存的中间态行。`lease_until = None` 表示 lease 已清。
async fn seed_record(
    pool: &SqlitePool,
    key: &str,
    state: &str,
    attempt_count: i64,
    lease_until: Option<OffsetDateTime>,
) -> i64 {
    let render_id = insert_rule(
        pool,
        "render",
        &format!("render-{key}"),
        &format!("rsha-{key}"),
    )
    .await;
    let policy_id = insert_rule(
        pool,
        "selection_policy",
        &format!("policy-{key}"),
        &format!("psha-{key}"),
    )
    .await;
    let repo = PublishRecordRepo::new(pool.clone());
    let id = repo
        .create_if_new(&NewPublishRecord {
            idempotency_key: key.to_string(),
            category_key: "ai".to_string(),
            report_date: "2026-06-10".to_string(),
            target_timezone: "Asia/Shanghai".to_string(),
            render_version: render_id,
            selection_policy_version: policy_id,
            remote_target: None,
        })
        .await
        .expect("create should succeed")
        .expect("record should be new");
    let lease_owner = lease_until.map(|_| "dead-worker".to_string());
    sqlx::query(
        "UPDATE publish_records SET state = ?, attempt_count = ?, stage_attempt_count = ?, lease_owner = ?, lease_expires_at = ? WHERE id = ?",
    )
    .bind(state)
    .bind(attempt_count)
    // Crash-recovery rows spent their attempts in the current stage (ADR 0012).
    .bind(attempt_count)
    .bind(lease_owner)
    .bind(lease_until)
    .bind(id)
    .execute(pool)
    .await
    .expect("seed update should succeed");
    id
}

async fn attempt_count(pool: &SqlitePool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT attempt_count FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("record should be readable")
}

async fn record_state(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("record should be readable")
}

fn claim_req(owner: &str) -> ClaimRequest {
    ClaimRequest {
        owner: owner.to_string(),
        now: now(),
        lease_expires_at: lease_expires_at(now(), Duration::seconds(30)),
        batch_size: 16,
        max_attempts: MAX_ATTEMPTS,
    }
}

#[tokio::test]
async fn each_cleared_lease_stage_state_is_claimable_only_by_its_stage() {
    let (_dir, pool) = make_test_pool().await;
    // 四个非终态行，attempt < max，lease 已清（崩溃在 advance 之后、下阶段 claim 之前）。
    let pending = seed_record(&pool, "p", "pending", 1, None).await;
    let frozen = seed_record(&pool, "f", "snapshot_frozen", 1, None).await;
    let rendered = seed_record(&pool, "r", "rendered", 1, None).await;
    let stored = seed_record(&pool, "s", "stored_local", 1, None).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let owner = build_owner_id();

    // 每个阶段 claim 只认自己的输入态——崩溃留存行被对应阶段精确续跑，互不串台。
    let ids = |rows: Vec<rss_ai_news_storage::ClaimedPublishRecord>| {
        rows.iter().map(|r| r.id).collect::<Vec<_>>()
    };
    let freeze = repo
        .claim_pending_for_freeze(&claim_req(&owner))
        .await
        .unwrap();
    assert_eq!(ids(freeze), vec![pending]);
    let render = repo
        .claim_frozen_for_render(&claim_req(&owner))
        .await
        .unwrap();
    assert_eq!(ids(render), vec![frozen]);
    let store = repo
        .claim_rendered_for_local_store(&claim_req(&owner))
        .await
        .unwrap();
    assert_eq!(ids(store), vec![rendered]);
    let remote = repo
        .claim_local_for_remote_publish(&claim_req(&owner))
        .await
        .unwrap();
    assert_eq!(ids(remote), vec![stored]);

    // claim 自增 attempt_count——崩溃恢复消耗共享预算，不会无限白嫖重试。
    assert_eq!(attempt_count(&pool, pending).await, 2);
    assert_eq!(attempt_count(&pool, stored).await, 2);
}

#[tokio::test]
async fn exhausted_stage_states_with_expired_lease_terminalize_after_reclaim() {
    let (_dir, pool) = make_test_pool().await;
    // 四个阶段态：预算耗尽 + 过期 lease（崩溃在持租阶段，预算已用完）。
    let expired = now() - Duration::seconds(1);
    let max = i64::from(MAX_ATTEMPTS);
    let pending = seed_record(&pool, "p", "pending", max, Some(expired)).await;
    let frozen = seed_record(&pool, "f", "snapshot_frozen", max, Some(expired)).await;
    let rendered = seed_record(&pool, "r", "rendered", max, Some(expired)).await;
    let stored = seed_record(&pool, "s", "stored_local", max, Some(expired)).await;
    let repo = PublishRecordRepo::new(pool.clone());

    // reclaim 清过期 lease（仅清 lease，不改 state）。
    let reclaimed = repo.reclaim_expired_leases(now()).await.unwrap();
    assert_eq!(reclaimed, 4);

    // sweep：预算耗尽 + lease 已清 → 四个阶段态全部收口 failed，无一卡死。
    let swept = repo
        .terminalize_exhausted(MAX_ATTEMPTS, now())
        .await
        .unwrap();
    assert_eq!(swept, 4);
    for id in [pending, frozen, rendered, stored] {
        assert_eq!(record_state(&pool, id).await, "failed");
    }
}

/// B13: SQLite stores leases as RFC3339 TEXT with a variable-length fraction,
/// so `'…20.5Z' < '…20Z'` holds lexicographically although 20.5 s is later.
/// Every lease predicate (claim, claim-by-ids, reclaim, terminalize sweep)
/// must compare instants: a lease ending half a second after `now` is live.
#[tokio::test]
async fn sqlite_lease_predicates_compare_instants_not_rfc3339_text() {
    let (_dir, pool) = make_test_pool().await;
    let live = seed_record(&pool, "subsec-live", "stored_local", 1, Some(now())).await;
    let exhausted = seed_record(
        &pool,
        "subsec-exhausted",
        "stored_local",
        i64::from(MAX_ATTEMPTS),
        Some(now()),
    )
    .await;
    // `now()` is a whole second; the leases end 0.5 s later, written as text
    // the way a fractional instant is stored.
    let half_second_later = "2023-11-14T22:13:20.5Z";
    assert_eq!(
        now() + Duration::milliseconds(500),
        OffsetDateTime::parse(
            half_second_later,
            &time::format_description::well_known::Rfc3339
        )
        .unwrap()
    );
    sqlx::query("UPDATE publish_records SET lease_expires_at = ? WHERE id IN (?, ?)")
        .bind(half_second_later)
        .bind(live)
        .bind(exhausted)
        .execute(&pool)
        .await
        .unwrap();
    let repo = PublishRecordRepo::new(pool.clone());

    assert_eq!(repo.reclaim_expired_leases(now()).await.unwrap(), 0);
    assert!(
        repo.claim_local_for_remote_publish(&claim_req("thief"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.claim_publish_by_ids(
            &claim_req("thief"),
            rss_ai_news_storage::PublishState::StoredLocal,
            &[live],
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        repo.terminalize_exhausted(MAX_ATTEMPTS, now())
            .await
            .unwrap(),
        0
    );
    assert_eq!(record_state(&pool, exhausted).await, "stored_local");
    assert_eq!(attempt_count(&pool, live).await, 1);

    // Once the instant has passed, the same rows are expired.
    let later = now() + Duration::seconds(1);
    assert_eq!(
        repo.terminalize_exhausted(MAX_ATTEMPTS, later)
            .await
            .unwrap(),
        1
    );
    assert_eq!(record_state(&pool, exhausted).await, "failed");
    assert_eq!(repo.reclaim_expired_leases(later).await.unwrap(), 1);
}

/// A claim stores its lease deadline rounded up to a whole millisecond (UTC),
/// so `julianday()` (millisecond resolution) never treats it as expired
/// before its instant.
#[tokio::test]
async fn sqlite_claim_lease_is_not_expired_before_its_instant() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_record(&pool, "subms", "stored_local", 0, None).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let claim_at = now() + Duration::nanoseconds(123_456_789);
    let lease_until = claim_at + Duration::seconds(30);
    let claimed = repo
        .claim_publish_by_ids(
            &ClaimRequest {
                owner: "holder".to_string(),
                now: claim_at,
                lease_expires_at: lease_until,
                batch_size: 1,
                max_attempts: MAX_ATTEMPTS,
            },
            rss_ai_news_storage::PublishState::StoredLocal,
            &[id],
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    assert_eq!(repo.reclaim_expired_leases(lease_until).await.unwrap(), 0);
    assert_eq!(
        repo.reclaim_expired_leases(lease_until + Duration::milliseconds(2))
            .await
            .unwrap(),
        1
    );
}

fn claim_at(at: OffsetDateTime, owner: &str) -> ClaimRequest {
    ClaimRequest {
        owner: owner.to_string(),
        now: at,
        lease_expires_at: lease_expires_at(at, Duration::seconds(30)),
        batch_size: 1,
        max_attempts: MAX_ATTEMPTS,
    }
}

/// R1-P-S12: a rate-limited release persists `next_attempt_at`; neither the
/// state-scan nor the by-id claim takes the record before that instant
/// (sub-second precise), both take it from then on, and the next advance
/// clears the wait. A plain retryable release clears it too.
#[tokio::test]
async fn sqlite_rate_limit_wait_is_honoured_by_every_claim_and_cleared_on_advance() {
    use rss_ai_news_storage::{PublishAdvanceExtras, PublishState, PublishTimestampField};
    let (_dir, pool) = make_test_pool().await;
    let id = seed_record(&pool, "wait", "stored_local", 0, None).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let first = now() + Duration::nanoseconds(100_000_000);
    assert_eq!(
        repo.claim_local_for_remote_publish(&claim_at(first, "w1"))
            .await
            .unwrap()
            .len(),
        1
    );
    let not_before = first + Duration::seconds(60) + Duration::nanoseconds(500_000_000);
    let outcome = repo
        .release_retryable_failure_until(
            id,
            "w1",
            "rate limited",
            "github_rate_limit",
            MAX_ATTEMPTS,
            Some(not_before),
            first,
        )
        .await
        .unwrap();
    assert!(outcome.released && !outcome.exhausted);

    let early = not_before - Duration::milliseconds(1);
    assert!(
        repo.claim_local_for_remote_publish(&claim_at(early, "w2"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.claim_publish_by_ids(&claim_at(early, "w2"), PublishState::StoredLocal, &[id])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        attempt_count(&pool, id).await,
        1,
        "a skipped claim costs nothing"
    );

    let claimed = repo
        .claim_publish_by_ids(
            &claim_at(not_before, "w3"),
            PublishState::StoredLocal,
            &[id],
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1, "claimable from the instant on");
    assert!(
        repo.release_advance(
            id,
            "w3",
            PublishState::StoredLocal,
            PublishState::PublishedRemote,
            PublishTimestampField::RemotePublishedAt,
            not_before,
            PublishAdvanceExtras::default(),
        )
        .await
        .unwrap()
    );
    let wait: Option<String> =
        sqlx::query_scalar("SELECT next_attempt_at FROM publish_records WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(wait, None, "advance clears the wait");

    // A retryable release without a wait clears an earlier one.
    let other = seed_record(&pool, "wait-clear", "rendered", 0, None).await;
    sqlx::query("UPDATE publish_records SET next_attempt_at = ? WHERE id = ?")
        .bind(first - Duration::seconds(1))
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    repo.claim_publish_by_ids(&claim_at(first, "w4"), PublishState::Rendered, &[other])
        .await
        .unwrap();
    repo.release_retryable_failure(other, "w4", "io", "local_io_error", MAX_ATTEMPTS, first)
        .await
        .unwrap();
    let wait: Option<String> =
        sqlx::query_scalar("SELECT next_attempt_at FROM publish_records WHERE id = ?")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(wait, None);
}

/// R3F-S15: the by-id freeze claim filters the category in SQL — a record of
/// another category is neither claimed nor touched; its own category claims.
#[tokio::test]
async fn sqlite_by_id_freeze_claim_requires_the_category() {
    let (_dir, pool) = make_test_pool().await;
    let id = seed_record(&pool, "by-id-category", "pending", 0, None).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let before = state_counters_lease(&pool, id).await;

    let other = repo
        .claim_pending_for_freeze_by_id_in_category(&claim_at(now(), "c1"), id, "ml")
        .await
        .unwrap();
    assert!(other.is_empty());
    assert_eq!(state_counters_lease(&pool, id).await, before);

    let own = repo
        .claim_pending_for_freeze_by_id_in_category(&claim_at(now(), "c2"), id, "ai")
        .await
        .unwrap();
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].category_key, "ai");
}

async fn state_counters_lease(pool: &SqlitePool, id: i64) -> (String, i64, i64, Option<String>) {
    sqlx::query_as(
        "SELECT state, attempt_count, stage_attempt_count, lease_owner FROM publish_records WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// R5-2: a deferred release records the unverified write (`Record`), a later
/// one keeps it (`Keep`), both kinds of claim project it, and
/// `Record(None)`, a charged retryable release and a stage advance each
/// clear it.
#[tokio::test]
async fn sqlite_deferred_retry_records_keeps_and_clears_the_unverified_write() {
    use rss_ai_news_domain::dto::publish::RemoteBlob;
    use rss_ai_news_storage::{
        PublishAdvanceExtras, PublishState, PublishTimestampField, TerminalAdvanceStatus,
        UnverifiedWriteUpdate,
    };
    let (_dir, pool) = make_test_pool().await;
    let id = seed_record(&pool, "recorded", "stored_local", 0, None).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let blob = RemoteBlob {
        path: "reports/ai/2026/20260610.md".to_string(),
        blob_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
    };
    let at = now();
    let by_id = |owner: &'static str| {
        let repo = repo.clone();
        async move {
            let claimed = repo
                .claim_publish_by_ids(&claim_at(at, owner), PublishState::StoredLocal, &[id])
                .await
                .unwrap();
            assert_eq!(claimed.len(), 1, "{owner}");
            claimed.into_iter().next().unwrap()
        }
    };
    async fn defer(
        repo: &PublishRecordRepo,
        id: i64,
        owner: &str,
        update: UnverifiedWriteUpdate<'_>,
        at: OffsetDateTime,
    ) {
        assert!(
            repo.release_deferred_retry(
                id,
                owner,
                PublishState::StoredLocal,
                "unverified",
                "remote_write_unverified",
                None,
                update,
                at,
            )
            .await
            .unwrap(),
            "{owner}"
        );
    }
    let columns = || {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Option<String>)>(
                "SELECT unverified_write_path, unverified_write_blob_sha FROM publish_records \
                 WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    assert_eq!(by_id("r1").await.unverified_write(), None);
    defer(
        &repo,
        id,
        "r1",
        UnverifiedWriteUpdate::Record(Some(&blob)),
        at,
    )
    .await;
    let scanned = repo
        .claim_local_for_remote_publish(&claim_at(at, "r2"))
        .await
        .unwrap();
    assert_eq!(scanned.len(), 1);
    assert_eq!(
        scanned[0].unverified_write(),
        Some(blob.clone()),
        "scan claim"
    );
    defer(&repo, id, "r2", UnverifiedWriteUpdate::Keep, at).await;
    assert_eq!(
        by_id("r3").await.unverified_write(),
        Some(blob.clone()),
        "kept, by-id claim"
    );
    defer(&repo, id, "r3", UnverifiedWriteUpdate::Record(None), at).await;
    assert_eq!(columns().await, (None, None), "Record(None) clears");

    by_id("r4").await;
    defer(
        &repo,
        id,
        "r4",
        UnverifiedWriteUpdate::Record(Some(&blob)),
        at,
    )
    .await;
    by_id("r5").await;
    let released = repo
        .release_retryable_failure(id, "r5", "refused", "github_api_error", MAX_ATTEMPTS, at)
        .await
        .unwrap();
    assert!(released.released && !released.exhausted);
    assert_eq!(columns().await, (None, None), "a charged release clears");

    by_id("r6").await;
    defer(
        &repo,
        id,
        "r6",
        UnverifiedWriteUpdate::Record(Some(&blob)),
        at,
    )
    .await;
    by_id("r7").await;
    let advanced = repo
        .release_terminal_advance_with_articles(
            id,
            "r7",
            PublishState::StoredLocal,
            PublishState::PublishedRemote,
            PublishTimestampField::RemotePublishedAt,
            Vec::new(),
            PublishAdvanceExtras::default(),
            at,
        )
        .await
        .unwrap();
    assert_eq!(advanced.status, TerminalAdvanceStatus::Advanced);
    assert_eq!(columns().await, (None, None), "the advance clears");
}

/// R5-3: recording a write before it is sent sets the recovery kind, path
/// and blob, keeps the lease and both counters, and is owner- and
/// state-guarded (a lost lease records nothing).
#[tokio::test]
async fn sqlite_record_remote_write_records_the_write_and_keeps_the_lease() {
    use rss_ai_news_domain::dto::publish::RemoteBlob;
    use rss_ai_news_storage::{PublishState, RecordedWrite};
    let (_dir, pool) = make_test_pool().await;
    let id = seed_record(&pool, "pending-write", "stored_local", 1, None).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let at = now();
    repo.claim_publish_by_ids(&claim_at(at, "w1"), PublishState::StoredLocal, &[id])
        .await
        .unwrap();
    let blob = RemoteBlob {
        path: "reports/ai.md".to_string(),
        blob_sha: "abc".to_string(),
    };
    for (owner, state) in [
        ("other", PublishState::StoredLocal),
        ("w1", PublishState::Rendered),
    ] {
        assert_eq!(
            repo.record_remote_write(id, owner, state, "x", "k", Some(&blob), None, at)
                .await
                .unwrap(),
            RecordedWrite::LeaseLost
        );
    }
    assert_eq!(
        repo.record_remote_write(
            id,
            "w1",
            PublishState::StoredLocal,
            "in flight",
            "remote_write_unverified",
            Some(&blob),
            None,
            at,
        )
        .await
        .unwrap(),
        RecordedWrite::Recorded
    );
    assert_eq!(
        state_counters_lease(&pool, id).await,
        ("stored_local".to_string(), 2, 2, Some("w1".to_string())),
        "lease and counters kept"
    );
    let recorded: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT last_error_kind, unverified_write_path, unverified_write_blob_sha \
         FROM publish_records WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        recorded,
        (
            Some("remote_write_unverified".to_string()),
            Some("reports/ai.md".to_string()),
            Some("abc".to_string())
        )
    );
    assert!(
        repo.owns_publish_records(&[id], "w1", PublishState::StoredLocal)
            .await
            .unwrap()
    );
    assert!(
        !repo
            .owns_publish_records(&[id], "other", PublishState::StoredLocal)
            .await
            .unwrap()
    );
}

/// `(category, remote_target, recorded path)` of `id`; a recorded path also
/// marks it `remote_write_unverified`.
async fn set_destination(
    pool: &SqlitePool,
    id: i64,
    state: &str,
    category: &str,
    remote_target: &str,
    path: Option<&str>,
) {
    sqlx::query(
        "UPDATE publish_records SET state = ?, category_key = ?, remote_target = ?, \
         last_error_kind = CASE WHEN ? IS NULL THEN NULL ELSE 'remote_write_unverified' END, \
         unverified_write_path = ?, lease_owner = NULL, lease_expires_at = NULL WHERE id = ?",
    )
    .bind(state)
    .bind(category)
    .bind(remote_target)
    .bind(path)
    .bind(path)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// R5-5 / R5-8: the holder of a remote destination is another category's
/// record published there, or another category's record whose write there
/// is unsettled for the same target — never the record itself or one of the
/// same category. Owner, repository, branch and path compare with ASCII
/// letters folded (GitHub owner/repo are case-insensitive), non-ASCII text
/// exactly; a held write records nothing.
#[tokio::test]
async fn sqlite_record_remote_write_is_held_by_another_categorys_destination() {
    use rss_ai_news_domain::dto::publish::RemoteBlob;
    use rss_ai_news_storage::{DestinationCheck, PublishState, RecordedWrite};
    let (_dir, pool) = make_test_pool().await;
    let repo = PublishRecordRepo::new(pool.clone());
    let me = seed_record(&pool, "me", "stored_local", 0, None).await;
    let other = seed_record(&pool, "other", "stored_local", 0, None).await;
    let at = now();
    let path = "报告/AI/É.md";
    let blob = RemoteBlob {
        path: path.to_string(),
        blob_sha: "abc".to_string(),
    };
    let check = DestinationCheck {
        category_key: "mine",
        target_identity: "o/r:main",
        receipt_target: "github://o/r/main/报告/AI/É.md",
    };
    let mut round = 0;
    let mut attempt = |expected: RecordedWrite, why: &'static str| {
        round += 1;
        let owner = format!("w{round}");
        let repo = repo.clone();
        let blob = blob.clone();
        async move {
            set_destination_mine(&repo, me, &owner).await;
            assert_eq!(
                repo.record_remote_write(
                    me,
                    &owner,
                    PublishState::StoredLocal,
                    "in flight",
                    "remote_write_unverified",
                    Some(&blob),
                    Some(check),
                    at,
                )
                .await
                .unwrap(),
                expected,
                "{why}"
            );
        }
    };
    async fn set_destination_mine(repo: &PublishRecordRepo, me: i64, owner: &str) {
        let claimed = repo
            .claim_publish_by_ids(&claim_at(now(), owner), PublishState::StoredLocal, &[me])
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "{owner}");
    }
    set_destination(&pool, me, "stored_local", "mine", "o/r:main", None).await;

    set_destination(
        &pool,
        other,
        "published_remote",
        "mine",
        check.receipt_target,
        None,
    )
    .await;
    attempt(RecordedWrite::Recorded, "same category").await;
    release(&repo, me).await;

    set_destination(
        &pool,
        other,
        "stored_local",
        "theirs",
        "o/x:main",
        Some(path),
    )
    .await;
    attempt(RecordedWrite::Recorded, "another target's write").await;
    release(&repo, me).await;

    set_destination(
        &pool,
        other,
        "stored_local",
        "theirs",
        "O/R:main",
        Some("报告/ai/É.md"),
    )
    .await;
    attempt(RecordedWrite::HeldBy(other), "unsettled write, other case").await;
    release(&repo, me).await;

    set_destination(
        &pool,
        other,
        "published_remote",
        "theirs",
        "github://O/R/main/报告/ai/É.md",
        None,
    )
    .await;
    attempt(
        RecordedWrite::HeldBy(other),
        "published there, other ASCII case",
    )
    .await;
    release(&repo, me).await;

    set_destination(
        &pool,
        other,
        "published_remote",
        "theirs",
        "github://o/r/main/报告/AI/é.md",
        None,
    )
    .await;
    attempt(RecordedWrite::Recorded, "non-ASCII letters are not folded").await;

    async fn release(repo: &PublishRecordRepo, me: i64) {
        // Owned by the last attempt; a lease reclaim frees it for the next.
        repo.reclaim_expired_leases(now() + Duration::hours(1))
            .await
            .unwrap();
        let _ = me;
    }
}

/// R5-7: two records of different categories race for one free path. Both
/// are parked behind a write lock a third connection holds; once it is
/// released, exactly one records its write and the other is held by it.
#[tokio::test]
async fn sqlite_concurrent_record_remote_write_clears_one_writer_per_destination() {
    use rss_ai_news_domain::dto::publish::RemoteBlob;
    use rss_ai_news_storage::{DestinationCheck, PublishState, RecordedWrite};
    let (_dir, pool) = common::make_test_pool_with_connections(4).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let first = seed_record(&pool, "race-a", "stored_local", 0, None).await;
    let second = seed_record(&pool, "race-b", "stored_local", 0, None).await;
    for (id, category) in [(first, "acat"), (second, "bcat")] {
        set_destination(&pool, id, "stored_local", category, "o/r:main", None).await;
    }
    let at = now();
    for (id, owner) in [(first, "wa"), (second, "wb")] {
        repo.claim_publish_by_ids(&claim_at(at, owner), PublishState::StoredLocal, &[id])
            .await
            .unwrap();
    }
    let mut locker = pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *locker)
        .await
        .unwrap();
    let contend = |id: i64, owner: &'static str, category: &'static str| {
        let repo = repo.clone();
        tokio::spawn(async move {
            let blob = RemoteBlob {
                path: "p.md".to_string(),
                blob_sha: "abc".to_string(),
            };
            repo.record_remote_write(
                id,
                owner,
                PublishState::StoredLocal,
                "in flight",
                "remote_write_unverified",
                Some(&blob),
                Some(DestinationCheck {
                    category_key: category,
                    target_identity: "o/r:main",
                    receipt_target: "github://o/r/main/p.md",
                }),
                at,
            )
            .await
            .unwrap()
        })
    };
    let a = contend(first, "wa", "acat");
    let b = contend(second, "wb", "bcat");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!a.is_finished() && !b.is_finished(), "both parked");
    sqlx::query("COMMIT").execute(&mut *locker).await.unwrap();
    drop(locker);
    let mut outcomes = vec![a.await.unwrap(), b.await.unwrap()];
    outcomes.sort_by_key(|outcome| format!("{outcome:?}"));
    assert!(
        outcomes == vec![RecordedWrite::HeldBy(first), RecordedWrite::Recorded]
            || outcomes == vec![RecordedWrite::HeldBy(second), RecordedWrite::Recorded],
        "{outcomes:?}"
    );
}

/// R3F-B2 / S12: a deferred retry keeps the stage state, clears the lease,
/// refunds the claim on both counters and persists the wait (a sub-millisecond
/// deadline is rounded up, never claimable early); a release for another
/// state or another owner changes nothing; `None` clears an earlier wait.
#[tokio::test]
async fn sqlite_deferred_retry_keeps_state_refunds_the_claim_and_persists_the_wait() {
    use rss_ai_news_storage::PublishState;
    let (_dir, pool) = make_test_pool().await;
    let id = seed_record(&pool, "deferred", "stored_local", 2, None).await;
    let repo = PublishRecordRepo::new(pool.clone());
    let first = now();
    let claimed = repo
        .claim_publish_by_ids(&claim_at(first, "d1"), PublishState::StoredLocal, &[id])
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        state_counters_lease(&pool, id).await,
        ("stored_local".to_string(), 3, 3, Some("d1".to_string()))
    );

    // Another state or another owner: nothing happens.
    assert!(
        !repo
            .release_deferred_retry(
                id,
                "d1",
                PublishState::Rendered,
                "x",
                "x",
                None,
                rss_ai_news_storage::UnverifiedWriteUpdate::Keep,
                first
            )
            .await
            .unwrap()
    );
    assert!(
        !repo
            .release_deferred_retry(
                id,
                "other",
                PublishState::StoredLocal,
                "x",
                "x",
                None,
                rss_ai_news_storage::UnverifiedWriteUpdate::Keep,
                first
            )
            .await
            .unwrap()
    );

    let not_before = first + Duration::seconds(30) + Duration::nanoseconds(400_000);
    assert!(
        repo.release_deferred_retry(
            id,
            "d1",
            PublishState::StoredLocal,
            "verification incomplete",
            "remote_write_unverified",
            Some(not_before),
            rss_ai_news_storage::UnverifiedWriteUpdate::Keep,
            first,
        )
        .await
        .unwrap()
    );
    assert_eq!(
        state_counters_lease(&pool, id).await,
        ("stored_local".to_string(), 2, 2, None),
        "state kept, lease cleared, claim refunded"
    );
    let early = not_before - Duration::nanoseconds(400_000);
    assert!(
        repo.claim_local_for_remote_publish(&claim_at(early, "d2"))
            .await
            .unwrap()
            .is_empty(),
        "the rounded-up wait is never claimable early"
    );
    let later = early + Duration::milliseconds(1);
    let again = repo
        .claim_publish_by_ids(&claim_at(later, "d3"), PublishState::StoredLocal, &[id])
        .await
        .unwrap();
    assert_eq!(again.len(), 1);
    // R4-1: the claim projection carries the kind of the last release.
    assert_eq!(
        again[0].last_error_kind.as_deref(),
        Some("remote_write_unverified")
    );
    assert!(
        repo.release_deferred_retry(
            id,
            "d3",
            PublishState::StoredLocal,
            "x",
            "y",
            None,
            rss_ai_news_storage::UnverifiedWriteUpdate::Keep,
            later
        )
        .await
        .unwrap()
    );
    let scanned = repo
        .claim_local_for_remote_publish(&claim_at(later, "d4"))
        .await
        .unwrap();
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].last_error_kind.as_deref(), Some("y"));
    assert!(
        repo.release_deferred_retry(
            id,
            "d4",
            PublishState::StoredLocal,
            "x",
            "y",
            None,
            rss_ai_news_storage::UnverifiedWriteUpdate::Keep,
            later
        )
        .await
        .unwrap()
    );
    let wait: Option<String> =
        sqlx::query_scalar("SELECT next_attempt_at FROM publish_records WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(wait, None, "None clears the wait");
}
