//! W11-P3-C-4 末尾：4 repo × 1 happy 路径双轨参数化（手动两条 #[tokio::test]
//! 对应同一 helper）。验证 SQLite/PG 两侧行为等价（无 rstest 依赖）。
//!
//! - SQLite 路径：默认 `cargo test` 即跑，复用 [`common::make_test_pool`]
//! - PG 路径：`#[ignore]`，复用 [`common::pg::make_pg_test_pool`]，需要 docker
//!
//! 覆盖 happy（每 repo 1 条，端到端写 → 读 roundtrip）：
//!   1. FeedSourceRepo.upsert → find_by_id
//!   2. ReindexJobRepo.insert_pending → find_by_id
//!   3. ArticleRepo.insert_or_get_by_content_hash → find_by_id
//!   4. PublishRecordRepo.create_if_new → find_by_id
//!
//! 测试代码组织：
//!   - `happy_*(pool: &StoragePool)` 为 backend-agnostic 主体
//!   - `sqlite_*` / `pg_*` 双壳测试只负责 fixture 注入

mod common;

use common::{make_test_pool, pg::make_pg_test_pool};
use rss_ai_news_domain::{
    model::FeedSource,
    state::{FeedKind, FeedSourceStatus},
};
use rss_ai_news_storage::{
    ArticleAiResultRepo, ArticleAiResultRepository, ArticleRepo, ArticleRepository, FeedEntryRepo,
    FeedEntryRepository, FeedSourceRepo, FeedSourceRepository, NewAiResult, NewArticle,
    NewFeedEntry, NewPublishRecord, NewRawArtifact, NewRunEvent, PublishRecordRepo,
    PublishRecordRepository, RawArtifactRepo, RawArtifactRepository, ReindexJobRepo,
    ReindexJobRepository, RuleVersionRepo, RuleVersionRepository, RunEventRepo, RunEventRepository,
    StoragePool,
};
use sqlx::Executor;
use time::OffsetDateTime;

// ── seed helper（backend-agnostic raw SQL；跨方言 SQL 等价） ────

/// INSERT 一条 rule_versions(status='superseded') 返回 id。
async fn seed_rule(pool: &StoragePool, kind: &str, tag: &str) -> i64 {
    let sql = "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
               VALUES ($1, $2, 'seed', $3, 'superseded') RETURNING id";
    let sha = format!("sha-{tag}");
    match pool {
        StoragePool::Sqlite(p) => sqlx::query_scalar::<_, i64>(sql)
            .bind(kind)
            .bind(tag)
            .bind(&sha)
            .fetch_one(p)
            .await
            .expect("seed rule sqlite"),
        StoragePool::Postgres(p) => sqlx::query_scalar::<_, i64>(sql)
            .bind(kind)
            .bind(tag)
            .bind(&sha)
            .fetch_one(p)
            .await
            .expect("seed rule pg"),
    }
}

/// INSERT 一条 feed_sources 返回 id。`config_version` FK 由调用方传 seed_rule
/// 的 id。
async fn seed_feed_source(pool: &StoragePool, source_key: &str, config_version: i64) -> i64 {
    let sql = "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
                  feed_kind, config_version) \
               VALUES ('ai', $1, 'AI Main', 'https://example.com/feed.xml', 'rss', $2) \
               RETURNING id";
    match pool {
        StoragePool::Sqlite(p) => sqlx::query_scalar::<_, i64>(sql)
            .bind(source_key)
            .bind(config_version)
            .fetch_one(p)
            .await
            .expect("seed feed_source sqlite"),
        StoragePool::Postgres(p) => sqlx::query_scalar::<_, i64>(sql)
            .bind(source_key)
            .bind(config_version)
            .fetch_one(p)
            .await
            .expect("seed feed_source pg"),
    }
}

async fn seed_feed_entry(pool: &StoragePool, source_id: i64, uid: &str) -> i64 {
    let sql = "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
                  title_raw, discovered_at, state, dedup_decision) \
               VALUES ($1, $2, $3, $4, 'title', $5, 'pending_fetch', 'fresh') RETURNING id";
    let link = format!("https://example.com/{uid}");
    let hash = format!("hash-{uid}");
    let now = OffsetDateTime::now_utc();
    match pool {
        StoragePool::Sqlite(p) => sqlx::query_scalar::<_, i64>(sql)
            .bind(source_id)
            .bind(uid)
            .bind(&link)
            .bind(&hash)
            .bind(now)
            .fetch_one(p)
            .await
            .expect("seed feed_entry sqlite"),
        StoragePool::Postgres(p) => sqlx::query_scalar::<_, i64>(sql)
            .bind(source_id)
            .bind(uid)
            .bind(&link)
            .bind(&hash)
            .bind(now)
            .fetch_one(p)
            .await
            .expect("seed feed_entry pg"),
    }
}

/// `SELECT COUNT(*)::BIGINT` 在 PG 上需要 cast；SQLite COUNT 默认就是 INTEGER。
/// 这条 raw probe 是为了 sanity check（不依赖 sqlx::Decode），用 Executor::execute。
async fn ensure_schema_ready(pool: &StoragePool) {
    let sql = "SELECT 1";
    match pool {
        StoragePool::Sqlite(p) => {
            p.execute(sql).await.expect("sqlite probe");
        }
        StoragePool::Postgres(p) => {
            p.execute(sql).await.expect("pg probe");
        }
    }
}

fn sample_feed_source(source_key: &str, config_version: i64) -> FeedSource {
    let now = OffsetDateTime::now_utc();
    FeedSource {
        id: 0,
        category_key: "ai".to_string(),
        source_key: source_key.to_string(),
        display_name: "sample".to_string(),
        feed_url: "https://example.com/feed.xml".to_string(),
        feed_kind: FeedKind::Rss,
        status: FeedSourceStatus::Active,
        priority: 10,
        etag: None,
        last_modified: None,
        last_fetched_at: None,
        last_success_at: None,
        consecutive_failures: 0,
        last_error: None,
        last_error_kind: None,
        config_version,
        created_at: now,
        updated_at: now,
    }
}

fn sample_article(
    content_hash: &str,
    origin_feed_entry_id: i64,
    extractor_version: i64,
) -> NewArticle {
    NewArticle {
        content_hash: content_hash.to_string(),
        canonical_link: "https://example.com/a".to_string(),
        title: "Article".to_string(),
        body_text: "body".to_string(),
        body_html_artifact_id: None,
        extractor_strategy: "readability".to_string(),
        extractor_version,
        content_quality: "high".to_string(),
        word_count: 1,
        origin_feed_entry_id,
    }
}

// ── 1) FeedSourceRepo.upsert happy ────────────────────────────

async fn happy_feed_source_upsert(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let rule_id = seed_rule(pool, "config", "fs-happy").await;
    let repo = FeedSourceRepo::new_with_storage(pool.clone());

    let src = sample_feed_source("upsert-happy", rule_id);
    let id = repo.upsert(&src).await.expect("upsert");
    let found = repo.find_by_id(id).await.expect("find").expect("present");
    assert_eq!(found.source_key, "upsert-happy");
    assert_eq!(found.feed_kind, FeedKind::Rss);
    assert_eq!(found.config_version, rule_id);
}

#[tokio::test]
async fn sqlite_happy_feed_source_upsert() {
    let (_dir, pool) = make_test_pool().await;
    happy_feed_source_upsert(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_feed_source_upsert() {
    let ctx = make_pg_test_pool().await;
    happy_feed_source_upsert(ctx.storage_pool()).await;
    ctx.cleanup().await;
}

// ── 2) ReindexJobRepo.insert_pending happy ─────────────────────

async fn happy_reindex_insert_pending(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let rule_id = seed_rule(pool, "extractor", "rj-happy").await;
    let repo = ReindexJobRepo::new_with_storage(pool.clone());

    let now = OffsetDateTime::now_utc();
    let id = repo
        .insert_pending("articles", rule_id, now)
        .await
        .expect("insert_pending");
    let job = repo.find_by_id(id).await.expect("find").expect("present");
    assert_eq!(job.target, "articles");
    assert_eq!(job.state, "pending");
    assert_eq!(job.rule_version_id, rule_id);
    assert_eq!(job.attempt_count, 0);
}

#[tokio::test]
async fn sqlite_happy_reindex_insert_pending() {
    let (_dir, pool) = make_test_pool().await;
    happy_reindex_insert_pending(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_reindex_insert_pending() {
    let ctx = make_pg_test_pool().await;
    happy_reindex_insert_pending(ctx.storage_pool()).await;
    ctx.cleanup().await;
}

// ── 3) ArticleRepo.insert_or_get_by_content_hash happy ────────

async fn happy_article_insert_then_find(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let config_rule = seed_rule(pool, "config", "art-happy").await;
    let extractor_rule = seed_rule(pool, "extractor", "art-happy").await;
    let source_id = seed_feed_source(pool, "art-happy", config_rule).await;
    let entry_id = seed_feed_entry(pool, source_id, "uid-art-happy").await;
    let repo = ArticleRepo::new_with_storage(pool.clone());

    let outcome = repo
        .insert_or_get_by_content_hash(&sample_article("hash-art-happy", entry_id, extractor_rule))
        .await
        .expect("insert");
    assert!(outcome.newly_created);
    let id = outcome.article_id;

    let found = repo.find_by_id(id).await.expect("find").expect("present");
    assert_eq!(found.content_hash, "hash-art-happy");
    assert_eq!(found.origin_feed_entry_id, entry_id);
}

#[tokio::test]
async fn sqlite_happy_article_insert_then_find() {
    let (_dir, pool) = make_test_pool().await;
    happy_article_insert_then_find(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_article_insert_then_find() {
    let ctx = make_pg_test_pool().await;
    happy_article_insert_then_find(ctx.storage_pool()).await;
    ctx.cleanup().await;
}

// ── 4) PublishRecordRepo.create_if_new happy ──────────────────

async fn happy_publish_record_create(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let render_rule = seed_rule(pool, "render", "pr-happy").await;
    let policy_rule = seed_rule(pool, "selection_policy", "pr-happy").await;
    let repo = PublishRecordRepo::new_with_storage(pool.clone());

    let item = NewPublishRecord {
        idempotency_key: "pr-happy-idem".to_string(),
        category_key: "ai".to_string(),
        report_date: "2026-05-17".to_string(),
        target_timezone: "UTC".to_string(),
        render_version: render_rule,
        selection_policy_version: policy_rule,
        remote_target: None,
    };
    let id = repo
        .create_if_new(&item)
        .await
        .expect("create_if_new")
        .expect("first inserts");

    // 二次返 None（ON CONFLICT DO NOTHING）
    let second = repo.create_if_new(&item).await.expect("create second");
    assert!(second.is_none());

    let found = repo.find_by_id(id).await.expect("find").expect("present");
    assert_eq!(found.idempotency_key, "pr-happy-idem");
    assert_eq!(found.state, "pending");
    assert_eq!(found.render_version, render_rule);
    assert_eq!(found.selection_policy_version, policy_rule);

    let by_key = repo
        .find_by_idempotency_key("pr-happy-idem")
        .await
        .expect("find_by_idempotency_key")
        .expect("present");
    assert_eq!(by_key.id, id);
}

#[tokio::test]
async fn sqlite_happy_publish_record_create() {
    let (_dir, pool) = make_test_pool().await;
    happy_publish_record_create(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_publish_record_create() {
    let ctx = make_pg_test_pool().await;
    happy_publish_record_create(ctx.storage_pool()).await;
    ctx.cleanup().await;
}

// ── codex P3-E-fix1 MEDIUM-1 修复：扩 dual_backend smoke 到 P3-E 5 个 repo ──
//
// 每个 repo 1 个 backend-agnostic happy：让 P4 全量 rstest 启动时不会同时
// 承担"发现 P3-E 差异" + "扩展测试框架" 两件事。

// 5) RuleVersionRepo.get_or_create happy

async fn happy_rule_version_get_or_create(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let repo = RuleVersionRepo::new_with_storage(pool.clone());

    let id = repo
        .get_or_create("extractor", "rv-dual-1", "first", "sha-rv-1")
        .await
        .expect("get_or_create first");
    assert!(id > 0);

    // 二次同 key：ON CONFLICT DO NOTHING + 兜底 SELECT 返同 id
    let id_again = repo
        .get_or_create("extractor", "rv-dual-1", "first-again", "sha-rv-1-again")
        .await
        .expect("get_or_create same key");
    assert_eq!(id, id_again);

    let active = repo
        .active_rule("extractor")
        .await
        .expect("active_rule")
        .expect("present");
    assert_eq!(active.id, id, "first row of a new kind is active");
}

#[tokio::test]
async fn sqlite_happy_rule_version_get_or_create() {
    let (_dir, pool) = make_test_pool().await;
    happy_rule_version_get_or_create(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_rule_version_get_or_create() {
    let ctx = make_pg_test_pool().await;
    happy_rule_version_get_or_create(ctx.storage_pool()).await;
    ctx.cleanup().await;
}

// 6) RawArtifactRepo.upsert_inline happy

async fn happy_raw_artifact_upsert_inline(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let repo = RawArtifactRepo::new_with_storage(pool.clone());

    let payload = NewRawArtifact {
        kind: "feed_payload".to_string(),
        artifact_key: "https://example.com/dual.xml".to_string(),
        content_encoding: "utf-8".to_string(),
        inline_body: b"<rss>dual</rss>".to_vec(),
        byte_size: 15,
        sha256: "sha-dual".to_string(),
        retention_policy: "ephemeral".to_string(),
        expires_at: None,
    };
    let id = repo.upsert_inline(&payload).await.expect("upsert_inline");

    let found = repo
        .find_by_key("feed_payload", "https://example.com/dual.xml")
        .await
        .expect("find_by_key")
        .expect("present");
    assert_eq!(found.id, id);
    assert_eq!(found.byte_size, 15);
    assert_eq!(
        found.inline_body.as_deref(),
        Some(b"<rss>dual</rss>".as_slice())
    );
}

#[tokio::test]
async fn sqlite_happy_raw_artifact_upsert_inline() {
    let (_dir, pool) = make_test_pool().await;
    happy_raw_artifact_upsert_inline(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_raw_artifact_upsert_inline() {
    let ctx = make_pg_test_pool().await;
    happy_raw_artifact_upsert_inline(ctx.storage_pool()).await;
    ctx.cleanup().await;
}

// 7) RunEventRepo.insert happy

async fn happy_run_event_insert(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let repo = RunEventRepo::new_with_storage(pool.clone());

    let id = repo
        .insert(&NewRunEvent {
            run_id: "run-dual".to_string(),
            trace_id: Some("trace-dual".to_string()),
            stage: "ingest".to_string(),
            severity: "info".to_string(),
            event_kind: "started".to_string(),
            target_kind: None,
            target_id: None,
            message: "msg".to_string(),
            context_json: None,
        })
        .await
        .expect("run_event insert");
    assert!(id > 0);
}

#[tokio::test]
async fn sqlite_happy_run_event_insert() {
    let (_dir, pool) = make_test_pool().await;
    happy_run_event_insert(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_run_event_insert() {
    let ctx = make_pg_test_pool().await;
    happy_run_event_insert(ctx.storage_pool()).await;
    ctx.cleanup().await;
}

// 8) FeedEntryRepo.insert_if_new + find_by_id happy

async fn happy_feed_entry_insert_then_find(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let config_rule = seed_rule(pool, "config", "fe-dual").await;
    let source_id = seed_feed_source(pool, "fe-dual", config_rule).await;
    let repo = FeedEntryRepo::new_with_storage(pool.clone());

    let entry = NewFeedEntry {
        source_id,
        feed_entry_uid: "uid-fe-dual".to_string(),
        normalized_link: "https://example.com/uid-fe-dual".to_string(),
        link_hash: "hash-fe-dual".to_string(),
        title_raw: "Title".to_string(),
        summary_raw: None,
        published_at: None,
        discovered_at: OffsetDateTime::now_utc(),
    };
    let id = repo
        .insert_if_new(&entry)
        .await
        .expect("insert_if_new")
        .expect("first inserts");

    let second = repo
        .insert_if_new(&entry)
        .await
        .expect("insert_if_new second");
    assert!(
        second.is_none(),
        "ON CONFLICT(source_id, feed_entry_uid) DO NOTHING returns None"
    );

    let found = repo.find_by_id(id).await.expect("find").expect("present");
    assert_eq!(found.state, "pending_fetch");
    assert_eq!(found.feed_entry_uid, "uid-fe-dual");
}

#[tokio::test]
async fn sqlite_happy_feed_entry_insert_then_find() {
    let (_dir, pool) = make_test_pool().await;
    happy_feed_entry_insert_then_find(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_feed_entry_insert_then_find() {
    let ctx = make_pg_test_pool().await;
    happy_feed_entry_insert_then_find(ctx.storage_pool()).await;
    ctx.cleanup().await;
}

// 9) ArticleAiResultRepo.insert_pending happy

async fn happy_article_ai_result_insert_pending(pool: &StoragePool) {
    ensure_schema_ready(pool).await;
    let config_rule = seed_rule(pool, "config", "ai-dual").await;
    let extractor_rule = seed_rule(pool, "extractor", "ai-dual").await;
    let prompt_rule = seed_rule(pool, "ai_prompt", "ai-dual").await;
    let schema_rule = seed_rule(pool, "ai_schema", "ai-dual").await;
    let source_id = seed_feed_source(pool, "ai-dual", config_rule).await;
    let entry_id = seed_feed_entry(pool, source_id, "uid-ai-dual").await;
    let article_repo = ArticleRepo::new_with_storage(pool.clone());
    let outcome = article_repo
        .insert_or_get_by_content_hash(&NewArticle {
            content_hash: "hash-ai-dual".to_string(),
            canonical_link: "https://example.com/a".to_string(),
            title: "t".to_string(),
            body_text: "b".to_string(),
            body_html_artifact_id: None,
            extractor_strategy: "readability".to_string(),
            extractor_version: extractor_rule,
            content_quality: "high".to_string(),
            word_count: 1,
            origin_feed_entry_id: entry_id,
        })
        .await
        .unwrap();
    let article_id = outcome.article_id;

    let repo = ArticleAiResultRepo::new_with_storage(pool.clone());
    let id = repo
        .insert_pending(&NewAiResult {
            article_id,
            prompt_version: prompt_rule,
            output_schema_version: schema_rule,
            model_id: "gpt-4".to_string(),
        })
        .await
        .expect("insert_pending")
        .expect("first inserts");
    assert!(id > 0);

    // 二次同 unique 四元组：ON CONFLICT DO NOTHING 返 None
    let second = repo
        .insert_pending(&NewAiResult {
            article_id,
            prompt_version: prompt_rule,
            output_schema_version: schema_rule,
            model_id: "gpt-4".to_string(),
        })
        .await
        .expect("insert_pending second");
    assert!(second.is_none());
}

#[tokio::test]
async fn sqlite_happy_article_ai_result_insert_pending() {
    let (_dir, pool) = make_test_pool().await;
    happy_article_ai_result_insert_pending(&StoragePool::Sqlite(pool)).await;
}

#[tokio::test]
#[ignore = "需要 docker daemon"]
async fn pg_happy_article_ai_result_insert_pending() {
    let ctx = make_pg_test_pool().await;
    happy_article_ai_result_insert_pending(ctx.storage_pool()).await;
    ctx.cleanup().await;
}
