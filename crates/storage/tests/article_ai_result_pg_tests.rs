//! W11-P3-E-3：[`ArticleAiResultRepo`] PG 分支冒烟。
//!
//! 覆盖：
//!   - `insert_pending` ON CONFLICT DO NOTHING + `claim_pending` 端到端
//!   - `insert_pending_and_advance_article` 跨表事务（INSERT + UPDATE articles）
//!   - `release_success_and_advance_article`：
//!     * keep=true + score >= min → ReadyForPublish
//!     * keep=false 且无 other succeeded → PublishSkipped（OTHER_SUCCEEDED
//!       EXISTS decode i32 PG 路径验证）
//!
//! 默认 `#[ignore]`，需要 docker。

mod common;

use common::pg::{PgTestContext, make_pg_test_pool};
use rss_ai_news_storage::{
    AiCompleteArticleAdvance, AiSuccessOutcome, ArticleAiResultRepo, ArticleAiResultRepository,
    ClaimRequest, NewAiResult, ShareSide, ShareStage,
};
use time::OffsetDateTime;

fn lease_expires(now: OffsetDateTime) -> OffsetDateTime {
    now + time::Duration::minutes(5)
}

async fn seed_prompt_rule(ctx: &PgTestContext, tag: &str) -> (i64, i64) {
    let prompt_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('ai_prompt', $1, 'p', $2, 'superseded') RETURNING id",
    )
    .bind(format!("prompt-{tag}"))
    .bind(format!("sha-prompt-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let schema_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('ai_schema', $1, 's', $2, 'superseded') RETURNING id",
    )
    .bind(format!("schema-{tag}"))
    .bind(format!("sha-schema-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    (prompt_id, schema_id)
}

/// seed feed_source + feed_entry + article（article state 可自定义）
async fn seed_article(ctx: &PgTestContext, tag: &str, article_state: &str) -> i64 {
    let config_rule: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('config', $1, 'c', $2, 'superseded') RETURNING id",
    )
    .bind(format!("cfg-{tag}"))
    .bind(format!("sha-cfg-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
            feed_kind, config_version) \
         VALUES ('ai', $1, 'AI Main', 'https://example.com/feed.xml', 'rss', $2) \
         RETURNING id",
    )
    .bind(format!("src-{tag}"))
    .bind(config_rule)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let entry_id: i64 = sqlx::query_scalar(
        "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
            title_raw, discovered_at, state, dedup_decision) \
         VALUES ($1, $2, $3, $4, 'title', $5, 'pending_fetch', 'fresh') RETURNING id",
    )
    .bind(source_id)
    .bind(format!("uid-{tag}"))
    .bind(format!("https://example.com/{tag}"))
    .bind(format!("link-hash-{tag}"))
    .bind(OffsetDateTime::now_utc())
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    let ext_id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
         VALUES ('extractor', $1, 'e', $2, 'superseded') RETURNING id",
    )
    .bind(format!("ext-{tag}"))
    .bind(format!("sha-ext-{tag}"))
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO articles (content_hash, canonical_link, title, body_text, \
            extractor_strategy, extractor_version, content_quality, origin_feed_entry_id, state) \
         VALUES ($1, 'https://example.com/a', 'title', 'body', 'readability', $2, 'high', $3, $4) \
         RETURNING id",
    )
    .bind(format!("hash-{tag}"))
    .bind(ext_id)
    .bind(entry_id)
    .bind(article_state)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap()
}

fn sample_outcome(keep: Option<bool>, score: Option<i32>) -> AiSuccessOutcome {
    AiSuccessOutcome {
        summary: "summary".to_string(),
        tags_json: "[]".to_string(),
        importance_score: score,
        keep_decision: keep,
        raw_response_artifact_id: None,
        tokens_in: Some(100),
        tokens_out: Some(50),
        cost_micro_usd: Some(1_000),
        latency_ms: Some(250),
    }
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_insert_pending_then_claim_then_release_success() {
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "claim").await;
    let article_id = seed_article(&ctx, "claim", "persisted").await;
    let repo = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());

    let item = NewAiResult {
        article_id,
        prompt_version: prompt_id,
        output_schema_version: schema_id,
        model_id: "gpt-4".to_string(),
    };
    let id = repo
        .insert_pending(&item)
        .await
        .expect("pg insert_pending")
        .expect("first inserts");

    // 二次 ON CONFLICT 返 None
    let second = repo.insert_pending(&item).await.expect("pg insert second");
    assert!(second.is_none());

    let now = OffsetDateTime::now_utc();
    let claimed = repo
        .claim_pending(
            &ClaimRequest {
                owner: "worker-A".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 4,
                max_attempts: 3,
            },
            "ai",
        )
        .await
        .expect("pg claim_pending");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, id);
    assert_eq!(claimed[0].article_id, article_id);

    let released = repo
        .release_success(
            id,
            "worker-A",
            sample_outcome(Some(true), Some(80)),
            "pg-eff-model",
            now,
        )
        .await
        .expect("pg release_success");
    assert!(released);

    ctx.cleanup().await;
}

/// codex P3-E-fix1 HIGH-2 修复：§6.4 SKIP LOCKED 确定性验证（仿 fix1 H2 模板）。
/// tx_a `SELECT FOR UPDATE` 锁住首个 pending AI 任务不提交 → 另一连接
/// `claim_pending` 必跳被锁行 + 拿后续 + 5s timeout 兜底（SKIP LOCKED 回归 →
/// 等行锁超时）。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_claim_pending_skips_explicitly_locked_row() {
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "skip-lock").await;
    let article_a = seed_article(&ctx, "skip-lock-a", "persisted").await;
    let article_b = seed_article(&ctx, "skip-lock-b", "persisted").await;
    let repo = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());

    // seed 两条 pending AI 任务（id 升序 = ORDER BY id ASC 命中顺序）
    let id_a = repo
        .insert_pending(&NewAiResult {
            article_id: article_a,
            prompt_version: prompt_id,
            output_schema_version: schema_id,
            model_id: "gpt-4".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    let id_b = repo
        .insert_pending(&NewAiResult {
            article_id: article_b,
            prompt_version: prompt_id,
            output_schema_version: schema_id,
            model_id: "gpt-4".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(id_a < id_b, "test depends on monotonic id ordering");

    // tx_a 独立连接锁 id_a 不提交
    let mut tx_a = ctx.pg_pool().begin().await.expect("begin tx_a");
    let locked: i64 =
        sqlx::query_scalar("SELECT id FROM article_ai_results WHERE id = $1 FOR UPDATE")
            .bind(id_a)
            .fetch_one(&mut *tx_a)
            .await
            .expect("tx_a FOR UPDATE on id_a");
    assert_eq!(locked, id_a);

    // worker B 调 claim_pending(batch_size=1)：SKIP LOCKED 跳 id_a 拿 id_b
    let now = OffsetDateTime::now_utc();
    let claimed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        repo.claim_pending(
            &ClaimRequest {
                owner: "worker-B".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 1,
                max_attempts: 3,
            },
            "ai",
        ),
    )
    .await
    .expect("claim_pending must return within 5s (else SKIP LOCKED regressed)")
    .expect("claim call");
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].id, id_b,
        "SKIP LOCKED must skip locked id_a and pick id_b"
    );

    tx_a.rollback().await.expect("rollback tx_a");
    let id_a_state: String =
        sqlx::query_scalar("SELECT state FROM article_ai_results WHERE id = $1")
            .bind(id_a)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
    assert_eq!(
        id_a_state, "pending",
        "id_a must remain pending (SKIP LOCKED only skipped)"
    );

    ctx.cleanup().await;
}

/// codex P3-E-fix1 HIGH-3 修复：跨表事务回滚分支实证。
///
/// `insert_pending_and_advance_article`：article 当前不是 'persisted'（如已
/// ai_pending / ai_done），UPDATE articles `WHERE id AND state='persisted'`
/// rows_affected=0 → 整段事务回滚 → 已 INSERT 的 article_ai_results 行被
/// 撤销 + outcome.ai_result_id=None + article_already_advanced=true。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_insert_pending_and_advance_rolls_back_when_article_not_persisted() {
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "rollback").await;
    // article 已经在 ai_pending（不是 persisted），UPDATE 谓词不命中
    let article_id = seed_article(&ctx, "rollback", "ai_pending").await;
    let repo = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());

    let now = OffsetDateTime::now_utc();
    let outcome = repo
        .insert_pending_and_advance_article(
            &NewAiResult {
                article_id,
                prompt_version: prompt_id,
                output_schema_version: schema_id,
                model_id: "gpt-4".to_string(),
            },
            30,
            now,
        )
        .await
        .expect("pg insert_pending_and_advance");
    assert!(
        outcome.ai_result_id.is_none(),
        "rollback path returns ai_result_id=None"
    );
    assert!(!outcome.article_advanced);
    assert!(
        outcome.article_already_advanced,
        "non-persisted article triggers article_already_advanced=true"
    );

    // 关键断言：INSERT 的 article_ai_results 行已回滚，不存在
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM article_ai_results \
         WHERE article_id = $1 AND prompt_version = $2 AND output_schema_version = $3 \
           AND model_id = 'gpt-4'",
    )
    .bind(article_id)
    .bind(prompt_id)
    .bind(schema_id)
    .fetch_one(ctx.pg_pool())
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "cross-table rollback must NOT leave orphan article_ai_results row"
    );

    // articles.state 仍是 'ai_pending'（未被 UPDATE 命中）
    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
        .bind(article_id)
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(state, "ai_pending");

    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_insert_pending_and_advance_article_atomically() {
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "advance").await;
    let article_id = seed_article(&ctx, "advance", "persisted").await;
    let repo = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());

    let now = OffsetDateTime::now_utc();
    let outcome = repo
        .insert_pending_and_advance_article(
            &NewAiResult {
                article_id,
                prompt_version: prompt_id,
                output_schema_version: schema_id,
                model_id: "gpt-4".to_string(),
            },
            30,
            now,
        )
        .await
        .expect("pg insert_pending_and_advance");
    assert!(outcome.ai_result_id.is_some());
    assert!(outcome.article_advanced);
    assert!(!outcome.article_already_advanced);

    // 验证 articles.state 已推进 persisted → ai_pending
    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
        .bind(article_id)
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(state, "ai_pending");

    ctx.cleanup().await;
}

/// C6：claim 只锁 `article_ai_results` 行（`FOR UPDATE OF aar`）。另一事务锁住
/// 所属 feed_sources / articles 行时，可领取的 AI 行仍须被领取，而不是被
/// SKIP LOCKED 当作已锁跳过（worker 会把空批次当成队列耗尽提前结束）。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_claim_is_not_hidden_by_locked_source_or_article_rows() {
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "lock-of").await;
    let article_id = seed_article(&ctx, "lock-of", "ai_pending").await;
    let repo = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());
    let id = repo
        .insert_pending(&NewAiResult {
            article_id,
            prompt_version: prompt_id,
            output_schema_version: schema_id,
            model_id: "gpt-4".to_string(),
        })
        .await
        .unwrap()
        .unwrap();

    // Connection 1 holds row locks on the article and its source (as an
    // ingest / publish transaction would) and keeps its transaction open.
    let mut locker = ctx.pg_pool().begin().await.unwrap();
    sqlx::query(
        "SELECT a.id FROM articles a \
         JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id \
         JOIN feed_sources fs ON fs.id = fe.source_id \
         WHERE a.id = $1 FOR UPDATE OF a, fs",
    )
    .bind(article_id)
    .fetch_one(&mut *locker)
    .await
    .unwrap();

    // Connection 2 claims.
    let now = OffsetDateTime::now_utc();
    let claimed = repo
        .claim_pending(
            &ClaimRequest {
                owner: "worker-B".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 4,
                max_attempts: 3,
            },
            "ai",
        )
        .await
        .expect("pg claim_pending");
    locker.rollback().await.unwrap();

    assert_eq!(
        claimed.len(),
        1,
        "locked parent rows must not hide the AI row"
    );
    assert_eq!(claimed[0].id, id);
    ctx.cleanup().await;
}

/// C9：PG 的 publish 窗口份额 SQL——fresh 列表（反连接 + 来源时间升序）、
/// fresh claim（来源时间升序，`FOR UPDATE OF aar`）与两个 batch=1 轮换信号。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_fresh_share_lists_claims_and_signals_by_source_time() {
    use rss_ai_news_storage::{ArticleRepo, ArticleRepository};

    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "fresh").await;
    let ai = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());
    let articles = ArticleRepo::new_with_storage(ctx.storage_pool().clone());
    let now = OffsetDateTime::now_utc();
    let since = now - time::Duration::hours(48);
    let window = rss_ai_news_storage::FreshWindow { since, until: now };
    let mut ids = Vec::new();
    // (tag, source time): old backlog, late fresh, early fresh, future (R2-S1:
    // after the window's end, so backlog).
    for (tag, at) in [
        ("f-old", now - time::Duration::days(10)),
        ("f-late", now - time::Duration::hours(1)),
        ("f-early", now - time::Duration::hours(40)),
        ("f-future", now + time::Duration::hours(2)),
    ] {
        let id = seed_article(&ctx, tag, "persisted").await;
        sqlx::query(
            "UPDATE feed_entries SET published_at = $1 \
             WHERE id = (SELECT origin_feed_entry_id FROM articles WHERE id = $2)",
        )
        .bind(at)
        .bind(id)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
        ids.push(id);
    }
    let identity = NewAiResult {
        article_id: 0,
        prompt_version: prompt_id,
        output_schema_version: schema_id,
        model_id: "gpt-4".to_string(),
    };
    let listed = articles
        .list_fresh_for_ai_task_gen("ai", &identity, window, 10)
        .await
        .expect("pg fresh list");
    assert_eq!(
        listed.iter().map(|c| c.article_id).collect::<Vec<_>>(),
        [ids[2], ids[1]],
        "in-window only, earliest source time first"
    );
    // R2-B1: the backlog list is the complement (old and future articles).
    let backlog = articles
        .list_backlog_for_ai_task_gen("ai", &identity, window, 10, 0)
        .await
        .expect("pg backlog list");
    assert_eq!(
        backlog
            .iter()
            .map(|c| (c.article_id, c.in_window))
            .collect::<Vec<_>>(),
        [(ids[0], false), (ids[3], false)]
    );
    assert_eq!(
        ai.last_served_side("ai", ShareStage::Claim).await.unwrap(),
        None
    );

    let mut tasks = Vec::new();
    for id in &ids {
        tasks.push(
            ai.insert_pending(&NewAiResult {
                article_id: *id,
                ..identity.clone()
            })
            .await
            .unwrap()
            .unwrap(),
        );
    }
    // The anti-join drops articles that already have a task of the identity;
    // the backlog list then reconciles them (in-window rows flagged).
    assert!(
        articles
            .list_fresh_for_ai_task_gen("ai", &identity, window, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let backlog = articles
        .list_backlog_for_ai_task_gen("ai", &identity, window, 10, 0)
        .await
        .unwrap();
    assert_eq!(
        backlog
            .iter()
            .map(|c| (c.article_id, c.in_window))
            .collect::<Vec<_>>(),
        [
            (ids[0], false),
            (ids[1], true),
            (ids[2], true),
            (ids[3], false)
        ]
    );

    let request = ClaimRequest {
        owner: "w".to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 1,
        max_attempts: 3,
    };
    // Batch size 1: no state → fresh first; the served side is recorded.
    let fresh = ai
        .claim_pending_shared(&request, "ai", since)
        .await
        .unwrap();
    assert_eq!(fresh.iter().map(|c| c.id).collect::<Vec<_>>(), [tasks[2]]);
    assert_eq!(
        ai.last_served_side("ai", ShareStage::Claim).await.unwrap(),
        Some(ShareSide::Fresh)
    );
    // Next batch leads with the backlog: the out-of-window task.
    let oldest = ai
        .claim_pending_shared(&request, "ai", since)
        .await
        .unwrap();
    assert_eq!(oldest.iter().map(|c| c.id).collect::<Vec<_>>(), [tasks[0]]);
    assert_eq!(
        ai.last_served_side("ai", ShareStage::Claim).await.unwrap(),
        Some(ShareSide::Backlog)
    );
    let fresh = ai
        .claim_pending_shared(&request, "ai", since)
        .await
        .unwrap();
    assert_eq!(fresh.iter().map(|c| c.id).collect::<Vec<_>>(), [tasks[1]]);
    // The future-dated task is backlog, not fresh (R2-S1).
    let future = ai
        .claim_pending_shared(&request, "ai", since)
        .await
        .unwrap();
    assert_eq!(future.iter().map(|c| c.id).collect::<Vec<_>>(), [tasks[3]]);
    ctx.cleanup().await;
}

/// C3：身份冲突时按已有行对账仍为 persisted 的 article（PG 加 FOR UPDATE 路径）。
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_insert_conflict_reconciles_persisted_article_with_existing_row() {
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "reconcile").await;
    let repo = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());
    let now = OffsetDateTime::now_utc();
    for (tag, row_state, keep, score, expected) in [
        ("rc-pending", "pending", None, None, "ai_pending"),
        (
            "rc-high",
            "succeeded",
            Some(1),
            Some(80),
            "ready_for_publish",
        ),
        ("rc-low", "succeeded", Some(1), Some(10), "ai_done"),
        ("rc-filtered", "filtered", Some(0), None, "publish_skipped"),
        ("rc-failed", "permanent_failed", None, None, "persisted"),
    ] {
        let article_id = seed_article(&ctx, tag, "persisted").await;
        sqlx::query(
            "INSERT INTO article_ai_results (article_id, prompt_version, output_schema_version, \
             model_id, state, keep_decision, importance_score) VALUES ($1, $2, $3, 'gpt-4', $4, $5, $6)",
        )
        .bind(article_id)
        .bind(prompt_id)
        .bind(schema_id)
        .bind(row_state)
        .bind(keep)
        .bind(score)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
        let item = NewAiResult {
            article_id,
            prompt_version: prompt_id,
            output_schema_version: schema_id,
            model_id: "gpt-4".to_string(),
        };
        let outcome = repo
            .insert_pending_and_advance_article(&item, 30, now)
            .await
            .expect("pg insert conflict");
        assert!(outcome.ai_result_id.is_none(), "{tag}");
        assert_eq!(outcome.reconciled, expected != "persisted", "{tag}");
        let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
            .bind(article_id)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
        assert_eq!(state, expected, "{tag}");
    }

    // R2-S3: this identity filtered, another identity kept it with score 90.
    let two = seed_article(&ctx, "rc-two-ids", "persisted").await;
    for (model, row_state, keep, score) in [
        ("gpt-4", "filtered", 0, 0),
        ("other-model", "succeeded", 1, 90),
    ] {
        sqlx::query(
            "INSERT INTO article_ai_results (article_id, prompt_version, output_schema_version, \
             model_id, state, keep_decision, importance_score) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(two)
        .bind(prompt_id)
        .bind(schema_id)
        .bind(model)
        .bind(row_state)
        .bind(keep)
        .bind(score)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
    }
    let outcome = repo
        .insert_pending_and_advance_article(
            &NewAiResult {
                article_id: two,
                prompt_version: prompt_id,
                output_schema_version: schema_id,
                model_id: "gpt-4".to_string(),
            },
            30,
            now,
        )
        .await
        .expect("pg reconcile with another success");
    assert!(outcome.reconciled);
    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
        .bind(two)
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(state, "ready_for_publish");

    // backfill：新插入推进 persisted，已发布 article 保持不变且插入保留。
    let persisted = seed_article(&ctx, "bf-persisted", "persisted").await;
    let published = seed_article(&ctx, "bf-published", "published").await;
    for (article_id, expected) in [(persisted, "ai_pending"), (published, "published")] {
        let outcome = repo
            .insert_backfill_pending(
                &NewAiResult {
                    article_id,
                    prompt_version: prompt_id,
                    output_schema_version: schema_id,
                    model_id: "gpt-4".to_string(),
                },
                now,
            )
            .await
            .expect("pg backfill insert");
        assert!(outcome.ai_result_id.is_some());
        let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
            .bind(article_id)
            .fetch_one(ctx.pg_pool())
            .await
            .unwrap();
        assert_eq!(state, expected);
    }

    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_release_success_and_advance_keep_ready_for_publish() {
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "ready").await;
    let article_id = seed_article(&ctx, "ready", "persisted").await;
    let repo = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());

    let now = OffsetDateTime::now_utc();
    let item = NewAiResult {
        article_id,
        prompt_version: prompt_id,
        output_schema_version: schema_id,
        model_id: "gpt-4".to_string(),
    };
    let advance_outcome = repo
        .insert_pending_and_advance_article(&item, 30, now)
        .await
        .unwrap();
    let ai_result_id = advance_outcome.ai_result_id.unwrap();

    // claim 拿 lease
    let _ = repo
        .claim_pending(
            &ClaimRequest {
                owner: "w".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 4,
                max_attempts: 3,
            },
            "ai",
        )
        .await
        .unwrap();

    // keep=true + score=80 >= min=50 → ReadyForPublish
    let release = repo
        .release_success_and_advance_article(
            ai_result_id,
            "w",
            sample_outcome(Some(true), Some(80)),
            "pg-eff-model",
            article_id,
            50,
            now,
        )
        .await
        .expect("pg release_success_and_advance");
    assert!(release.released);
    assert_eq!(
        release.article_advance,
        AiCompleteArticleAdvance::ReadyForPublish
    );

    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
        .bind(article_id)
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(state, "ready_for_publish");

    ctx.cleanup().await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_release_success_and_advance_filtered_publish_skipped() {
    // keep=false 且无其他 succeeded ai_result → PublishSkipped；同时验证
    // OTHER_SUCCEEDED_AI_EXISTS_SQL（CASE WHEN EXISTS decode i32）在 PG 上工作
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "skipped").await;
    let article_id = seed_article(&ctx, "skipped", "persisted").await;
    let repo = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());

    let now = OffsetDateTime::now_utc();
    let item = NewAiResult {
        article_id,
        prompt_version: prompt_id,
        output_schema_version: schema_id,
        model_id: "gpt-4".to_string(),
    };
    let advance_outcome = repo
        .insert_pending_and_advance_article(&item, 30, now)
        .await
        .unwrap();
    let ai_result_id = advance_outcome.ai_result_id.unwrap();
    let _ = repo
        .claim_pending(
            &ClaimRequest {
                owner: "w".to_string(),
                now,
                lease_expires_at: lease_expires(now),
                batch_size: 4,
                max_attempts: 3,
            },
            "ai",
        )
        .await
        .unwrap();

    // keep=false（filtered），无其他 succeeded → PublishSkipped
    let release = repo
        .release_success_and_advance_article(
            ai_result_id,
            "w",
            sample_outcome(Some(false), None),
            "pg-eff-model",
            article_id,
            50,
            now,
        )
        .await
        .expect("pg release filtered");
    assert!(release.released);
    assert_eq!(
        release.article_advance,
        AiCompleteArticleAdvance::PublishSkipped
    );

    let state: String = sqlx::query_scalar("SELECT state FROM articles WHERE id = $1")
        .bind(article_id)
        .fetch_one(ctx.pg_pool())
        .await
        .unwrap();
    assert_eq!(state, "publish_skipped");

    ctx.cleanup().await;
}
/// R2-S2 on PG: fresh claims order by source time, ties by **article** id
/// (not task id), and the batch comes back in that order.
#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_fresh_claim_orders_by_source_time_then_article_id() {
    let ctx = make_pg_test_pool().await;
    let (prompt_id, schema_id) = seed_prompt_rule(&ctx, "order").await;
    let ai = ArticleAiResultRepo::new_with_storage(ctx.storage_pool().clone());
    let now = OffsetDateTime::now_utc();
    let mut ids = Vec::new();
    for (tag, hours) in [("o-a", 1), ("o-b", 2), ("o-c", 2)] {
        let id = seed_article(&ctx, tag, "ai_pending").await;
        sqlx::query(
            "UPDATE feed_entries SET published_at = $1 \
             WHERE id = (SELECT origin_feed_entry_id FROM articles WHERE id = $2)",
        )
        .bind(now - time::Duration::hours(hours))
        .bind(id)
        .execute(ctx.pg_pool())
        .await
        .unwrap();
        ids.push(id);
    }
    // Tasks created in reverse: article C owns the lowest task id.
    for id in ids.iter().rev() {
        ai.insert_pending(&NewAiResult {
            article_id: *id,
            prompt_version: prompt_id,
            output_schema_version: schema_id,
            model_id: "gpt-4".to_string(),
        })
        .await
        .unwrap()
        .unwrap();
    }
    let request = ClaimRequest {
        owner: "w".to_string(),
        now,
        lease_expires_at: lease_expires(now),
        batch_size: 3,
        max_attempts: 3,
    };

    let claimed = ai
        .claim_pending_shared(&request, "ai", now - time::Duration::hours(48))
        .await
        .unwrap();

    assert_eq!(
        claimed.iter().map(|c| c.article_id).collect::<Vec<_>>(),
        [ids[1], ids[2], ids[0]]
    );
    ctx.cleanup().await;
}
