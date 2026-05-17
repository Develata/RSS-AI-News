//! W11-P3-C-2：[`ReindexJobRepo`] PG 分支冒烟 + §8.4 PG 并发竞争。
//!
//! 覆盖：
//!   - happy：`start_reindex_tx` → `claim_by_id` → `finish_reindex_tx`
//!     端到端推进，验证 rule_versions demote/promote 顺序保留
//!   - 并发：两个并发 `claim_by_id` 同一 pending job 只一个成功
//!   - 并发：worker A 持 lease，B 抢先 abort，A 后续 advance_to_completed
//!     返 false（lease guard 失败，整段事务不变）
//!
//! 默认 `#[ignore]`，需要 docker。

mod common;

use std::sync::Arc;

use common::pg::{PgTestContext, make_pg_test_pool};
use rss_ai_news_storage::{
    FinishReindexTxOutcome, ReindexJobRepo, ReindexJobRepository, StartReindexTxOutcome,
};
use time::OffsetDateTime;

fn lease_expires(now: OffsetDateTime) -> OffsetDateTime {
    now + time::Duration::minutes(5)
}

/// 直接在 PG 上 INSERT 一条 rule_versions(status='superseded') 给 start_reindex_tx
/// 测试做"已有 active 行"基线（finish_reindex_tx 时被 demote）。返回 id。
async fn seed_active_rule(ctx: &PgTestContext, kind: &str, tag: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ($1, $2, 'baseline', $3, 'active') RETURNING id",
    )
    .bind(kind)
    .bind(tag)
    .bind(format!("sha-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .expect("seed active rule_versions")
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_start_then_claim_then_finish_end_to_end() {
    let ctx = make_pg_test_pool().await;
    let baseline_active_rule_id = seed_active_rule(&ctx, "extractor", "baseline").await;
    let repo = ReindexJobRepo::new_with_storage(ctx.storage_pool().clone());

    let now = OffsetDateTime::now_utc();

    // 1) start：两 INSERT 同事务
    let StartReindexTxOutcome {
        rule_version_id,
        job_id,
    } = repo
        .start_reindex_tx(
            "extractor",
            "v2",
            "Bumped extractor",
            "sha-v2",
            "articles",
            now,
        )
        .await
        .expect("pg start_reindex_tx");
    assert!(rule_version_id > 0 && job_id > 0);

    // 2) claim_by_id：pending → running，attempt_count 自增
    let claimed = repo
        .claim_by_id(job_id, "worker-1", now, lease_expires(now))
        .await
        .expect("pg claim_by_id")
        .expect("pending job present");
    assert_eq!(claimed.id, job_id);
    assert_eq!(claimed.rule_version_id, rule_version_id);
    assert_eq!(claimed.attempt_count, 1);

    // 3) finish_reindex_tx：job→completed + 旧 active demote + new pending → active
    let outcome = repo
        .finish_reindex_tx(job_id, "worker-1", rule_version_id, "extractor", now)
        .await
        .expect("pg finish_reindex_tx");
    assert_eq!(
        outcome,
        FinishReindexTxOutcome {
            job_completed: true,
            demoted_rule_version_id: Some(baseline_active_rule_id),
        }
    );

    // 验证新 active 真的活了
    let active_kind_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM rule_versions \
         WHERE kind = 'extractor' AND status = 'active'",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    assert_eq!(active_kind_count, 1, "exactly one active row per kind");
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_concurrent_claim_by_id_only_one_winner() {
    // §8.4 用例 1：两个 worker 同时 claim_by_id 同一 pending job，
    // SKIP LOCKED 保证一个拿到（rows_affected=1，返 Some）另一个 None。
    let ctx = make_pg_test_pool().await;
    // baseline active 行的存在不影响 claim 路径，仅用于确认 schema 完整
    let _ = seed_active_rule(&ctx, "extractor", "baseline").await;

    // 直接 INSERT pending job（不走 start_reindex_tx，避免 active rule 干扰）
    let rule_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', 'concurrent-1', 'c', 'sha', 'pending') RETURNING id",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let now = OffsetDateTime::now_utc();
    let job_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO reindex_jobs (target, rule_version_id, state, attempt_count, created_at, updated_at) \
         VALUES ('articles', $1, 'pending', 0, $2, $2) RETURNING id",
    )
    .bind(rule_id)
    .bind(now)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();

    let repo = Arc::new(ReindexJobRepo::new_with_storage(ctx.storage_pool().clone()));
    let lease = lease_expires(now);

    let repo_a = repo.clone();
    let repo_b = repo.clone();

    let (res_a, res_b) = tokio::join!(
        async move { repo_a.claim_by_id(job_id, "owner-A", now, lease).await },
        async move { repo_b.claim_by_id(job_id, "owner-B", now, lease).await }
    );

    let claimed_a = res_a.expect("worker A claim_by_id call");
    let claimed_b = res_b.expect("worker B claim_by_id call");

    // 仅一个成功
    let winners = [claimed_a.is_some(), claimed_b.is_some()]
        .iter()
        .filter(|x| **x)
        .count();
    assert_eq!(
        winners, 1,
        "exactly one worker should win the claim; got A={:?} B={:?}",
        claimed_a, claimed_b
    );

    // 验证 attempt_count == 1（只递增一次）
    let job = repo.find_by_id(job_id).await.unwrap().expect("job present");
    assert_eq!(job.state, "running");
    assert_eq!(
        job.attempt_count, 1,
        "attempt_count must increment exactly once"
    );
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_lease_guard_loses_to_concurrent_abort() {
    // §8.4 用例 2：worker A 持 lease（running），worker B 调 abort
    //（abort 不要求持 lease），A 再调 advance_to_completed 必返 false
    //（lease guard：state='running' AND lease_owner=A 不再成立）。
    let ctx = make_pg_test_pool().await;
    let rule_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', 'lease-race', 'r', 'sha', 'pending') RETURNING id",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let now = OffsetDateTime::now_utc();
    let job_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO reindex_jobs (target, rule_version_id, state, attempt_count, created_at, updated_at) \
         VALUES ('articles', $1, 'pending', 0, $2, $2) RETURNING id",
    )
    .bind(rule_id)
    .bind(now)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();

    let repo = ReindexJobRepo::new_with_storage(ctx.storage_pool().clone());

    // A claim 持 lease
    let claimed = repo
        .claim_by_id(job_id, "worker-A", now, lease_expires(now))
        .await
        .unwrap()
        .expect("worker A claim succeeds");
    assert_eq!(claimed.id, job_id);

    // B 抢先 abort（state: running → aborted）
    let aborted = repo
        .abort(job_id, "user requested", now)
        .await
        .expect("abort call");
    assert!(aborted, "abort applied (state was 'running')");

    // A 再 advance_to_completed：lease guard 失败 → false
    let completed = repo
        .advance_to_completed(job_id, "worker-A", now)
        .await
        .expect("advance_to_completed call");
    assert!(
        !completed,
        "advance_to_completed must fail after concurrent abort (state != 'running')"
    );

    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(
        job.state, "aborted",
        "job remains in aborted terminal state"
    );
    assert_eq!(job.aborted_reason.as_deref(), Some("user requested"));
}

/// codex P3-C 评审 HIGH-2 修复：§6.4 `FOR UPDATE SKIP LOCKED` 真实证据。
///
/// 原 `pg_concurrent_claim_by_id_only_one_winner` 即便 SKIP LOCKED 被移除
/// 也会通过——`state='pending'` 谓词在第二个 UPDATE 时已经看到第一个 UPDATE
/// 提交后的 `running`，自然返 None。这是**假阳性**。
///
/// 本测试以**确定性**方式证明 SKIP LOCKED 真在跳锁：
///
///   1. seed 两个 pending job（不同 target，partial unique 允许），
///      job_a.created_at < job_b.created_at，按 `ORDER BY created_at ASC`
///      claim_pending 默认应该拿 job_a。
///   2. tx_a 在独立连接显式 `SELECT id FROM reindex_jobs WHERE id=job_a
///      FOR UPDATE` 锁住 job_a 不提交。
///   3. 在另一个连接调 `claim_pending`：
///      - 没有 SKIP LOCKED：会等 tx_a 释放（行锁），整段超时
///      - 有 SKIP LOCKED：跳过 job_a，按 ORDER 拿 job_b
///   4. 断言 claimed.id == job_b，且 tx_a 释放后 job_a 仍是 pending。
///
/// SKIP LOCKED 一旦回归（被去掉或语法失效），claim_pending 要么超时要么
/// 拿到 job_a（因为 tx_a 持锁会让另一连接等待提交后看到 running——但
/// claim_pending 在并发场景下也可能在 tx_a 释放后看到原 pending）。无论
/// 哪种偏差，断言都会 fail。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_claim_pending_skip_locked_skips_explicitly_locked_row() {
    let ctx = make_pg_test_pool().await;
    let rule_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', 'skip-locked-rule', 'r', 'sha', 'pending') RETURNING id",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();

    let now = OffsetDateTime::now_utc();
    let earlier = now - time::Duration::seconds(10);

    // job_a 先 created（按 ORDER BY created_at ASC 应优先 claim）
    let job_a = sqlx::query_scalar::<_, i64>(
        "INSERT INTO reindex_jobs (target, rule_version_id, state, attempt_count, created_at, updated_at) \
         VALUES ('target-a', $1, 'pending', 0, $2, $2) RETURNING id",
    )
    .bind(rule_id)
    .bind(earlier)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();

    let job_b = sqlx::query_scalar::<_, i64>(
        "INSERT INTO reindex_jobs (target, rule_version_id, state, attempt_count, created_at, updated_at) \
         VALUES ('target-b', $1, 'pending', 0, $2, $2) RETURNING id",
    )
    .bind(rule_id)
    .bind(now)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();

    // tx_a 在独立连接上 SELECT job_a FOR UPDATE 占行锁不提交
    let mut tx_a = ctx.pg_pool().begin().await.expect("begin tx_a");
    let locked: i64 = sqlx::query_scalar("SELECT id FROM reindex_jobs WHERE id = $1 FOR UPDATE")
        .bind(job_a)
        .fetch_one(&mut *tx_a)
        .await
        .expect("tx_a SELECT FOR UPDATE on job_a");
    assert_eq!(locked, job_a);

    // worker B 调 claim_pending：必须跳过 job_a 拿 job_b（SKIP LOCKED 真生效）。
    // 包一层 timeout 兜底：如果 SKIP LOCKED 回归 → claim_pending 会等 tx_a 释放 →
    // 测试卡住直至 timeout，更容易定位。
    let repo = ReindexJobRepo::new_with_storage(ctx.storage_pool().clone());
    let claimed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        repo.claim_pending("worker-B", now, lease_expires(now)),
    )
    .await
    .expect("claim_pending must return within 5s (else SKIP LOCKED regressed)")
    .expect("claim_pending call")
    .expect("must claim a job (job_b)");
    assert_eq!(
        claimed.id, job_b,
        "SKIP LOCKED must skip locked job_a and pick next candidate job_b (got id={})",
        claimed.id
    );

    // 释放 tx_a，验证 job_a 仍是 pending（未被 worker B claim）
    tx_a.rollback().await.expect("rollback tx_a");
    let job_a_state: String = sqlx::query_scalar("SELECT state FROM reindex_jobs WHERE id = $1")
        .bind(job_a)
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(
        job_a_state, "pending",
        "job_a must remain pending (SKIP LOCKED only skipped it, did not claim)"
    );
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_reclaim_expired_leases_clears_owner_and_returns_to_pending() {
    let ctx = make_pg_test_pool().await;
    let rule_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', 'reclaim-test', 'r', 'sha', 'pending') RETURNING id",
    )
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let now = OffsetDateTime::now_utc();
    let stale_lease = now - time::Duration::seconds(1);
    let job_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO reindex_jobs (target, rule_version_id, state, lease_owner, \
            lease_expires_at, last_processed_id, attempt_count, started_at, created_at, updated_at) \
         VALUES ('articles', $1, 'running', 'dead-worker', $2, 42, 1, $3, $3, $3) RETURNING id",
    )
    .bind(rule_id)
    .bind(stale_lease)
    .bind(now)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();

    let repo = ReindexJobRepo::new_with_storage(ctx.storage_pool().clone());
    let reclaimed = repo
        .reclaim_expired_leases(now)
        .await
        .expect("pg reclaim_expired_leases");
    assert_eq!(reclaimed, 1);

    let job = repo.find_by_id(job_id).await.unwrap().unwrap();
    assert_eq!(job.state, "pending", "stale-lease job back to pending");
    assert_eq!(job.lease_owner, None);
    assert_eq!(job.lease_expires_at, None);
    assert_eq!(
        job.last_processed_id,
        Some(42),
        "checkpoint preserved across reclaim"
    );
}
