mod common;

use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use rss_ai_news_config::RetentionPolicy;
use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::feed::FeedFetchRequest;
use rss_ai_news_feed::{FeedError, FeedFetcher};
use rss_ai_news_runtime::{
    PublishFlow, PublishFreezeOptions, PublishFreezeStatus, PublishInitOptions, PublishInitOutcome,
};
use rss_ai_news_storage::{PublishItemRepo, PublishItemRepository};
use sqlx::SqlitePool;

use common::{
    app_config, full_context, insert_config_rule, make_test_pool, seed_ai_succeeded_article,
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
    assert_record_state(&pool, publish_record_id, "failed").await;
    let kind: Option<String> =
        sqlx::query_scalar("SELECT last_error_kind FROM publish_records WHERE id = ?")
            .bind(publish_record_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind.as_deref(), Some("snapshot_empty"));
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
    assert_record_state(&pool, publish_record_id, "failed").await;
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
    sqlx::query("UPDATE publish_records SET attempt_count = 5 WHERE id = ?")
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
    let ctx = Arc::new(full_context(
        "publish",
        pool,
        app,
        Arc::new(DummyFeedFetcher),
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

struct DummyFeedFetcher;

#[async_trait]
impl FeedFetcher for DummyFeedFetcher {
    async fn fetch_raw(
        &self,
        _request: &FeedFetchRequest,
    ) -> Result<rss_ai_news_feed::fetcher::RawFeedFetch, FeedError> {
        Err(FeedError::ConnectionFailed {
            source: "publish tests do not fetch feeds".to_string(),
        })
    }
}
