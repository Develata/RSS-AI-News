mod common;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::Score0To100;
use rss_ai_news_runtime::{
    PublishFlow, PublishFreezeOptions, PublishFreezeStatus, PublishInitOptions, PublishInitOutcome,
};
use rss_ai_news_storage::{
    FreezeSnapshotItem, FreezeSnapshotOutcome, PublishCandidateRow, PublishItemRepo,
    PublishItemRepository, StorageError,
};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use common::{
    app_config, insert_config_rule, make_test_pool, publish_deps, seed_ai_succeeded_article,
    seed_persisted_article_for_passthrough,
};

#[tokio::test]
async fn init_creates_publish_record_returns_created_outcome() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let render = insert_config_rule(&pool).await;
    let policy = insert_config_rule(&pool).await;

    let outcome = flow
        .init(init_opts(render, policy))
        .await
        .expect("init should succeed");

    assert!(matches!(outcome, PublishInitOutcome::Created { .. }));
}

#[tokio::test]
async fn init_returns_already_exists_on_idempotency_key_conflict() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let render = insert_config_rule(&pool).await;
    let policy = insert_config_rule(&pool).await;
    let opts = init_opts(render, policy);
    flow.init(opts.clone()).await.unwrap();

    let outcome = flow.init(opts).await.expect("second init should succeed");

    assert!(matches!(
        outcome,
        PublishInitOutcome::AlreadyExists {
            state,
            ..
        } if state == "pending"
    ));
}

#[tokio::test]
async fn freeze_with_ai_path_inserts_publish_items_and_advances_record_to_snapshot_frozen() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;
    let (article_id, ai_result_id) =
        seed_ai_succeeded_article(&pool, "ai", "runtime-ai", "Title", "body", "summary", 88, 1)
            .await;

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.publish_record_id, publish_record_id);
    assert_eq!(outcome.status, PublishFreezeStatus::Frozen);
    assert_eq!(outcome.item_count, 1);
    assert_record_state(&pool, publish_record_id, "snapshot_frozen").await;
    let items = PublishItemRepo::new(pool.clone())
        .list_by_publish_record(publish_record_id)
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].article_id, article_id);
    assert_eq!(items[0].article_ai_result_id, Some(ai_result_id));
}

#[tokio::test]
async fn freeze_record_claims_requested_pending_record_not_older_one() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let render = insert_config_rule(&pool).await;
    let policy = insert_config_rule(&pool).await;
    let stale_id = match flow
        .init(PublishInitOptions {
            category_key: "stale".to_string(),
            report_date: "2026-04-27".to_string(),
            target_timezone: "Asia/Shanghai".to_string(),
            render_version: render,
            selection_policy_version: policy,
            remote_target: None,
        })
        .await
        .unwrap()
    {
        PublishInitOutcome::Created { publish_record_id } => publish_record_id,
        PublishInitOutcome::AlreadyExists { .. } => panic!("stale key should be unique"),
    };
    let publish_record_id = init_record(&flow, &pool).await;
    seed_ai_succeeded_article(
        &pool,
        "ai",
        "runtime-ai-by-id",
        "Title",
        "body",
        "summary",
        88,
        1,
    )
    .await;

    let outcome = flow
        .freeze_record(publish_record_id, freeze_opts(true, false))
        .await;

    assert_eq!(outcome.publish_record_id, publish_record_id);
    assert_eq!(outcome.status, PublishFreezeStatus::Frozen);
    assert_record_state(&pool, stale_id, "pending").await;
    assert_record_state(&pool, publish_record_id, "snapshot_frozen").await;
}

#[tokio::test]
async fn freeze_record_isolates_two_concurrent_pending_records_by_id() {
    // 回归：9080223 修复的 race —— 之前 publish-all 串行 freeze 时第二个分类的
    // freeze 调 claim_pending_for_freeze() 会按状态扫表抢任意 pending record，
    // 可能反向抢到上一个分类还未完成的那条。修复后 freeze_record(id) 必须严格
    // 按 id 命中目标 record，不会跨抢同 state 的其它 record。本测试断言两条同
    // state pending 记录在双向调用下各自只被对应 id 推进，且 items 分配只属于
    // 自己的 category。
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let render = insert_config_rule(&pool).await;
    let policy = insert_config_rule(&pool).await;
    let id_a = init_for_category(&flow, "ai", "2026-04-28", render, policy).await;
    let id_b = init_for_category(&flow, "ml", "2026-04-28", render, policy).await;
    let (article_a, _) =
        seed_ai_succeeded_article(&pool, "ai", "concurrent-ai", "TA", "ba", "sa", 88, 1).await;
    let (article_b, _) =
        seed_ai_succeeded_article(&pool, "ml", "concurrent-ml", "TB", "bb", "sb", 88, 1).await;

    let outcome_a = flow
        .freeze_record(id_a, freeze_opts_for_category("ai"))
        .await;
    assert_eq!(outcome_a.publish_record_id, id_a);
    assert_eq!(outcome_a.status, PublishFreezeStatus::Frozen);
    assert_record_state(&pool, id_a, "snapshot_frozen").await;
    assert_record_state(&pool, id_b, "pending").await;

    let outcome_b = flow
        .freeze_record(id_b, freeze_opts_for_category("ml"))
        .await;
    assert_eq!(outcome_b.publish_record_id, id_b);
    assert_eq!(outcome_b.status, PublishFreezeStatus::Frozen);
    assert_record_state(&pool, id_a, "snapshot_frozen").await;
    assert_record_state(&pool, id_b, "snapshot_frozen").await;

    let item_repo = PublishItemRepo::new(pool.clone());
    let items_a = item_repo.list_by_publish_record(id_a).await.unwrap();
    let items_b = item_repo.list_by_publish_record(id_b).await.unwrap();
    assert_eq!(
        items_a.len(),
        1,
        "record A should hold ai-side article only"
    );
    assert_eq!(
        items_b.len(),
        1,
        "record B should hold ml-side article only"
    );
    assert_eq!(items_a[0].article_id, article_a);
    assert_eq!(items_b[0].article_id, article_b);
}

/// R1-P-S15: the no-ID freeze selects for `opts.category_key`, so it claims
/// only that category's pending record — never an older record of another
/// category (whose snapshot would then hold this category's articles).
#[tokio::test]
async fn freeze_without_id_claims_only_its_own_categorys_record() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let render = insert_config_rule(&pool).await;
    let policy = insert_config_rule(&pool).await;
    let older_ai = init_for_category(&flow, "ai", "2026-04-28", render, policy).await;
    let ml = init_for_category(&flow, "ml", "2026-04-28", render, policy).await;
    let (ml_article, _) =
        seed_ai_succeeded_article(&pool, "ml", "own-cat-ml", "T", "b", "s", 88, 1).await;

    let outcome = flow.freeze(freeze_opts_for_category("ml")).await;

    assert_eq!(outcome.publish_record_id, ml);
    assert_eq!(outcome.status, PublishFreezeStatus::Frozen);
    assert_record_state(&pool, older_ai, "pending").await;
    let items = PublishItemRepo::new(pool.clone())
        .list_by_publish_record(ml)
        .await
        .unwrap();
    assert_eq!(
        items.iter().map(|item| item.article_id).collect::<Vec<_>>(),
        vec![ml_article]
    );
}

/// R1-P-S15: the by-id entry refuses a record of another category instead
/// of freezing `opts.category_key`'s articles into it; the claim is refunded.
#[tokio::test]
async fn freeze_record_rejects_a_record_of_another_category() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let render = insert_config_rule(&pool).await;
    let policy = insert_config_rule(&pool).await;
    let ai = init_for_category(&flow, "ai", "2026-04-28", render, policy).await;
    seed_ai_succeeded_article(&pool, "ml", "mismatch-ml", "T", "b", "s", 88, 1).await;

    let outcome = flow.freeze_record(ai, freeze_opts_for_category("ml")).await;

    assert!(
        matches!(&outcome.status, PublishFreezeStatus::Failed { error_kind } if error_kind == "category_mismatch"),
        "{:?}",
        outcome.status
    );
    assert_record_state(&pool, ai, "pending").await;
    let (attempts, owned, owner): (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT attempt_count, (SELECT COUNT(*) FROM publish_items WHERE publish_record_id = ?), \
         lease_owner FROM publish_records WHERE id = ?",
    )
    .bind(ai)
    .bind(ai)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((attempts, owned, owner), (0, 0, None));
}

#[tokio::test]
async fn freeze_with_ai_off_passthrough_promotes_persisted_articles_in_same_tx() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;
    let article_id =
        seed_persisted_article_for_passthrough(&pool, "ai", "runtime-direct", "Title", "raw").await;

    let outcome = flow.freeze(freeze_opts(false, true)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::Frozen);
    assert_record_state(&pool, publish_record_id, "snapshot_frozen").await;
    assert_article_state(&pool, article_id, "ready_for_publish").await;
}

#[tokio::test]
async fn freeze_returns_snapshot_empty_when_no_candidates_match() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::SnapshotEmpty);
    // Empty is not a failure: back to claimable pending, attempt refunded.
    assert_record_state(&pool, publish_record_id, "pending").await;
    let (kind, attempts, lease_owner): (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT last_error_kind, attempt_count, lease_owner FROM publish_records WHERE id = ?",
    )
    .bind(publish_record_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kind.as_deref(), Some("snapshot_empty"));
    assert_eq!(attempts, 0);
    assert_eq!(lease_owner, None);
}

#[tokio::test]
async fn freeze_reports_failed_when_the_empty_snapshot_release_fails() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;
    // Make exactly the empty-snapshot release UPDATE fail.
    sqlx::query(
        "CREATE TRIGGER fail_empty_release BEFORE UPDATE ON publish_records \
         WHEN NEW.last_error_kind = 'snapshot_empty' \
         BEGIN SELECT RAISE(ABORT, 'injected'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    // Not reported as a harmless skip: the record was not released.
    assert!(
        matches!(outcome.status, PublishFreezeStatus::Failed { .. }),
        "{:?}",
        outcome.status
    );
    assert_eq!(outcome.publish_record_id, publish_record_id);
}

#[tokio::test]
async fn freeze_reports_conflicted_when_the_empty_snapshot_release_loses_the_lease() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;
    // RAISE(IGNORE) skips the row: the release affects 0 rows, as when the
    // lease was taken over by another worker.
    sqlx::query(
        "CREATE TRIGGER lose_empty_release BEFORE UPDATE ON publish_records \
         WHEN NEW.last_error_kind = 'snapshot_empty' \
         BEGIN SELECT RAISE(IGNORE); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::Conflicted);
    assert_eq!(outcome.publish_record_id, publish_record_id);
}

#[tokio::test]
async fn freeze_after_empty_snapshot_publishes_articles_that_arrive_later() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;

    // Repeated empty runs never exhaust the retry budget.
    for _ in 0..10 {
        let outcome = flow.freeze(freeze_opts(true, false)).await;
        assert_eq!(outcome.status, PublishFreezeStatus::SnapshotEmpty);
    }
    seed_ai_succeeded_article(&pool, "ai", "late-ai", "Title", "body", "summary", 88, 1).await;

    let outcome = flow.freeze(freeze_opts(true, false)).await;
    assert_eq!(outcome.status, PublishFreezeStatus::Frozen);
    assert_eq!(outcome.publish_record_id, publish_record_id);
    assert_record_state(&pool, publish_record_id, "snapshot_frozen").await;
}

#[tokio::test]
async fn freeze_returns_nothing_to_claim_when_no_pending_records() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool);

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::NothingToClaim);
    assert_eq!(outcome.publish_record_id, 0);
}

#[tokio::test]
async fn freeze_skips_articles_without_correct_category_key() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let publish_record_id = init_record(&flow, &pool).await;
    seed_ai_succeeded_article(&pool, "ml", "runtime-ml", "Title", "body", "summary", 88, 1).await;

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(outcome.status, PublishFreezeStatus::SnapshotEmpty);
    assert_record_state(&pool, publish_record_id, "pending").await;
}

// === W15: freeze 入口启动期 maintenance（docs/plan/15 §5） ===

#[tokio::test]
async fn freeze_run_start_maintenance_sweeps_exhausted_pending_record() {
    // W15 §5：freeze 是 publish 表的 CLI 入口之一，首次 claim 前 sweep——
    // 预算耗尽（attempt_count >= publish_max_attempts=5）的 pending 记录转
    // failed。改前语义：claim 过滤 attempt_count < max 永远跳过它，行无限滞留。
    let (_dir, pool) = make_test_pool().await;
    let flow = flow(pool.clone());
    let exhausted_id = init_record(&flow, &pool).await;
    sqlx::query(
        "UPDATE publish_records SET attempt_count = 5, stage_attempt_count = 5 WHERE id = ?",
    )
    .bind(exhausted_id)
    .execute(&pool)
    .await
    .expect("prime exhausted attempt_count");

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    // 唯一记录已被 sweep 转终态，claim 拿不到 → NothingToClaim。
    assert_eq!(outcome.status, PublishFreezeStatus::NothingToClaim);
    assert_record_state(&pool, exhausted_id, "failed").await;
    let (severity, context): (String, String) = sqlx::query_as(
        "SELECT severity, context_json FROM run_events WHERE event_kind = 'retry_budget_swept'",
    )
    .fetch_one(&pool)
    .await
    .expect("retry_budget_swept event should exist");
    assert_eq!(severity, "warn");
    assert!(context.contains(r#""table":"publish_records""#));
}

fn flow(pool: SqlitePool) -> PublishFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let ctx = Arc::new(publish_deps(
        pool,
        app,
        Arc::new(rss_ai_news_publish::LocalFsTarget::new(std::env::temp_dir())),
        None,
    ));
    PublishFlow::new(ctx)
}

async fn init_record(flow: &PublishFlow, pool: &SqlitePool) -> i64 {
    let render = insert_config_rule(pool).await;
    let policy = insert_config_rule(pool).await;
    match flow.init(init_opts(render, policy)).await.unwrap() {
        PublishInitOutcome::Created { publish_record_id } => publish_record_id,
        PublishInitOutcome::AlreadyExists { .. } => panic!("test key should be unique"),
    }
}

fn init_opts(render_version: i64, selection_policy_version: i64) -> PublishInitOptions {
    PublishInitOptions {
        category_key: "ai".to_string(),
        report_date: "2026-04-28".to_string(),
        target_timezone: "Asia/Shanghai".to_string(),
        render_version,
        selection_policy_version,
        remote_target: None,
    }
}

fn freeze_opts(ai_enabled: bool, include_unscored: bool) -> PublishFreezeOptions {
    PublishFreezeOptions {
        category_key: "ai".to_string(),
        max_items: NonZeroU32::new(10).unwrap(),
        min_importance_score: Score0To100::try_new(50).unwrap(),
        include_unscored,
        ai_enabled,
        candidate_window_hours: 48,
        excerpt_max_chars: 100,
    }
}

async fn init_for_category(
    flow: &PublishFlow,
    category_key: &str,
    report_date: &str,
    render_version: i64,
    selection_policy_version: i64,
) -> i64 {
    let outcome = flow
        .init(PublishInitOptions {
            category_key: category_key.to_string(),
            report_date: report_date.to_string(),
            target_timezone: "Asia/Shanghai".to_string(),
            render_version,
            selection_policy_version,
            remote_target: None,
        })
        .await
        .expect("init should succeed");
    match outcome {
        PublishInitOutcome::Created { publish_record_id } => publish_record_id,
        PublishInitOutcome::AlreadyExists { .. } => {
            panic!("init key should be unique for {category_key} on {report_date}")
        }
    }
}

fn freeze_opts_for_category(category_key: &str) -> PublishFreezeOptions {
    PublishFreezeOptions {
        category_key: category_key.to_string(),
        max_items: NonZeroU32::new(10).unwrap(),
        min_importance_score: Score0To100::try_new(50).unwrap(),
        include_unscored: false,
        ai_enabled: true,
        candidate_window_hours: 48,
        excerpt_max_chars: 100,
    }
}

async fn assert_record_state(pool: &SqlitePool, id: i64, expected: &str) {
    let state: String = sqlx::query_scalar("SELECT state FROM publish_records WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, expected);
}

async fn assert_article_state(pool: &SqlitePool, id: i64, expected: &str) {
    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(state, expected);
}

// === B3: candidate-load storage errors keep their classification ===

/// Delegates to the real repo but fails the first AI-path candidate read with
/// `err`, the way a pool timeout or a dropped connection would.
struct FlakyCandidateRepo {
    inner: PublishItemRepo,
    failures_left: AtomicUsize,
    err: fn() -> StorageError,
}

#[async_trait::async_trait]
impl PublishItemRepository for FlakyCandidateRepo {
    async fn select_ai_path_candidates(
        &self,
        category_key: &str,
        min_importance_score: i32,
        published_since: OffsetDateTime,
        published_until: OffsetDateTime,
        max_items: NonZeroU32,
    ) -> Result<Vec<PublishCandidateRow>, StorageError> {
        if self
            .failures_left
            .fetch_update(AtomicOrdering::SeqCst, AtomicOrdering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err((self.err)());
        }
        self.inner
            .select_ai_path_candidates(
                category_key,
                min_importance_score,
                published_since,
                published_until,
                max_items,
            )
            .await
    }

    async fn select_ai_off_passthrough_candidates(
        &self,
        category_key: &str,
        published_since: OffsetDateTime,
        published_until: OffsetDateTime,
        max_items: NonZeroU32,
    ) -> Result<Vec<PublishCandidateRow>, StorageError> {
        self.inner
            .select_ai_off_passthrough_candidates(
                category_key,
                published_since,
                published_until,
                max_items,
            )
            .await
    }

    async fn freeze_snapshot(
        &self,
        publish_record_id: i64,
        owner: &str,
        items: Vec<FreezeSnapshotItem>,
        ai_off_promote_article_ids: Vec<i64>,
        now: OffsetDateTime,
    ) -> Result<FreezeSnapshotOutcome, StorageError> {
        self.inner
            .freeze_snapshot(
                publish_record_id,
                owner,
                items,
                ai_off_promote_article_ids,
                now,
            )
            .await
    }

    async fn list_by_publish_record(
        &self,
        publish_record_id: i64,
    ) -> Result<Vec<rss_ai_news_domain::model::PublishItem>, StorageError> {
        self.inner.list_by_publish_record(publish_record_id).await
    }

    async fn find_snapshot_ownership_conflict(
        &self,
        publish_record_id: i64,
    ) -> Result<Option<rss_ai_news_storage::SnapshotOwnershipConflict>, StorageError> {
        self.inner
            .find_snapshot_ownership_conflict(publish_record_id)
            .await
    }
}

fn flaky_flow(pool: SqlitePool, err: fn() -> StorageError) -> PublishFlow {
    let app = Arc::new(app_config(RetentionPolicy::Always, 1));
    let mut deps = publish_deps(
        pool.clone(),
        app,
        Arc::new(rss_ai_news_publish::LocalFsTarget::new(std::env::temp_dir())),
        None,
    );
    deps.publish_item_repo = Arc::new(FlakyCandidateRepo {
        inner: PublishItemRepo::new(pool),
        failures_left: AtomicUsize::new(1),
        err,
    });
    PublishFlow::new(Arc::new(deps))
}

#[tokio::test]
async fn freeze_transient_candidate_load_error_keeps_record_pending_and_next_run_freezes() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flaky_flow(pool.clone(), || StorageError::Timeout);
    let publish_record_id = init_record(&flow, &pool).await;
    seed_ai_succeeded_article(&pool, "ai", "flaky-ai", "Title", "body", "summary", 88, 1).await;

    let first = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(first.publish_record_id, publish_record_id);
    assert!(
        matches!(&first.status, PublishFreezeStatus::Failed { error_kind } if error_kind == "db_timeout"),
        "{:?}",
        first.status
    );
    // A transient DB failure is not a business verdict: the record stays
    // claimable, the lease is released, and the attempt is charged to the
    // retry budget.
    let (state, kind, attempts, lease_owner): (String, Option<String>, i64, Option<String>) =
        sqlx::query_as(
            "SELECT state, last_error_kind, attempt_count, lease_owner \
             FROM publish_records WHERE id = ?",
        )
        .bind(publish_record_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(kind.as_deref(), Some("db_timeout"));
    assert_eq!(attempts, 1);
    assert_eq!(lease_owner, None);

    let second = flow.freeze(freeze_opts(true, false)).await;

    assert_eq!(second.status, PublishFreezeStatus::Frozen);
    assert_eq!(second.publish_record_id, publish_record_id);
    assert_record_state(&pool, publish_record_id, "snapshot_frozen").await;
}

#[tokio::test]
async fn freeze_permanent_candidate_load_error_fails_record() {
    let (_dir, pool) = make_test_pool().await;
    let flow = flaky_flow(pool.clone(), || {
        StorageError::Corruption("bad row".to_string())
    });
    let publish_record_id = init_record(&flow, &pool).await;
    seed_ai_succeeded_article(&pool, "ai", "corrupt-ai", "Title", "body", "summary", 88, 1).await;

    let outcome = flow.freeze(freeze_opts(true, false)).await;

    assert!(
        matches!(&outcome.status, PublishFreezeStatus::Failed { error_kind } if error_kind == "corruption"),
        "{:?}",
        outcome.status
    );
    assert_record_state(&pool, publish_record_id, "failed").await;
}

// === B2 (ADR 0012): a freeze that loses an article releases at once ===

/// Real repo whose first AI-path selection is immediately followed by another
/// worker freezing the top candidate into its own record (a concurrent
/// freeze between selection and this freeze's transaction).
struct RacingFreezeRepo {
    inner: PublishItemRepo,
    pool: SqlitePool,
    rival_record: i64,
    raced: AtomicUsize,
}

#[async_trait::async_trait]
impl PublishItemRepository for RacingFreezeRepo {
    async fn select_ai_path_candidates(
        &self,
        category_key: &str,
        min_importance_score: i32,
        published_since: OffsetDateTime,
        published_until: OffsetDateTime,
        max_items: NonZeroU32,
    ) -> Result<Vec<PublishCandidateRow>, StorageError> {
        let rows = self
            .inner
            .select_ai_path_candidates(
                category_key,
                min_importance_score,
                published_since,
                published_until,
                max_items,
            )
            .await?;
        if self.raced.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            let top = &rows[0];
            sqlx::query(
                "INSERT INTO publish_items (publish_record_id, position, article_id, \
                 article_ai_result_id, frozen_title, frozen_summary, frozen_tags_json, \
                 frozen_score, frozen_canonical_link, frozen_source_display_name) \
                 VALUES (?, 1, ?, ?, 't', 's', '[]', 88, 'https://example.com', 'src')",
            )
            .bind(self.rival_record)
            .bind(top.article_id)
            .bind(top.article_ai_result_id)
            .execute(&self.pool)
            .await
            .unwrap();
            sqlx::query("UPDATE publish_records SET state = 'snapshot_frozen' WHERE id = ?")
                .bind(self.rival_record)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        Ok(rows)
    }

    async fn select_ai_off_passthrough_candidates(
        &self,
        category_key: &str,
        published_since: OffsetDateTime,
        published_until: OffsetDateTime,
        max_items: NonZeroU32,
    ) -> Result<Vec<PublishCandidateRow>, StorageError> {
        self.inner
            .select_ai_off_passthrough_candidates(
                category_key,
                published_since,
                published_until,
                max_items,
            )
            .await
    }

    async fn freeze_snapshot(
        &self,
        publish_record_id: i64,
        owner: &str,
        items: Vec<FreezeSnapshotItem>,
        ai_off_promote_article_ids: Vec<i64>,
        now: OffsetDateTime,
    ) -> Result<FreezeSnapshotOutcome, StorageError> {
        self.inner
            .freeze_snapshot(
                publish_record_id,
                owner,
                items,
                ai_off_promote_article_ids,
                now,
            )
            .await
    }

    async fn list_by_publish_record(
        &self,
        publish_record_id: i64,
    ) -> Result<Vec<rss_ai_news_domain::model::PublishItem>, StorageError> {
        self.inner.list_by_publish_record(publish_record_id).await
    }

    async fn find_snapshot_ownership_conflict(
        &self,
        publish_record_id: i64,
    ) -> Result<Option<rss_ai_news_storage::SnapshotOwnershipConflict>, StorageError> {
        self.inner
            .find_snapshot_ownership_conflict(publish_record_id)
            .await
    }
}

#[tokio::test]
async fn freeze_losing_an_article_releases_lease_without_charging_budget_and_retries_without_it() {
    let (_dir, pool) = make_test_pool().await;
    // A lost race must not exhaust even a single-attempt stage budget.
    let mut app = app_config(RetentionPolicy::Always, 1);
    app.retry.publish_max_attempts = 1;
    let mut deps = publish_deps(
        pool.clone(),
        Arc::new(app),
        Arc::new(rss_ai_news_publish::LocalFsTarget::new(std::env::temp_dir())),
        None,
    );
    let rival_flow = flow(pool.clone());
    let rival_render = insert_config_rule(&pool).await;
    let rival_policy = insert_config_rule(&pool).await;
    let rival_record =
        init_for_category(&rival_flow, "ai", "2026-04-27", rival_render, rival_policy).await;
    deps.publish_item_repo = Arc::new(RacingFreezeRepo {
        inner: PublishItemRepo::new(pool.clone()),
        pool: pool.clone(),
        rival_record,
        raced: AtomicUsize::new(0),
    });
    let racing = PublishFlow::new(Arc::new(deps));
    let record = init_record(&racing, &pool).await;
    let (contested, _) =
        seed_ai_succeeded_article(&pool, "ai", "contested-ai", "Top", "body", "summary", 95, 1)
            .await;
    let (other, _) =
        seed_ai_succeeded_article(&pool, "ai", "other-ai", "Next", "body", "summary", 70, 1).await;

    let first = racing.freeze_record(record, freeze_opts(true, false)).await;

    assert_eq!(
        first.status,
        PublishFreezeStatus::ArticleConflict {
            article_id: contested
        }
    );
    let (state, kind, attempts, stage_attempts, lease_owner): (
        String,
        Option<String>,
        i64,
        i64,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT state, last_error_kind, attempt_count, stage_attempt_count, lease_owner \
         FROM publish_records WHERE id = ?",
    )
    .bind(record)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(kind.as_deref(), Some("article_owned_by_active_snapshot"));
    assert_eq!((attempts, stage_attempts), (0, 0), "claim refunded");
    assert_eq!(lease_owner, None, "the lease is never held on a conflict");

    let second = racing.freeze_record(record, freeze_opts(true, false)).await;

    assert_eq!(second.status, PublishFreezeStatus::Frozen);
    let items = PublishItemRepo::new(pool.clone())
        .list_by_publish_record(record)
        .await
        .unwrap();
    assert_eq!(
        items.iter().map(|item| item.article_id).collect::<Vec<_>>(),
        vec![other]
    );
}
