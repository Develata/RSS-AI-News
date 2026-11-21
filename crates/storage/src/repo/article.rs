use async_trait::async_trait;
use rss_ai_news_domain::model::Article;
use rss_ai_news_domain::state::{ArticleState, ContentQuality, ExtractorStrategy};
use sqlx::{FromRow, PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, StoragePool, classify_db_error};

use super::feed_entry_sql::{
    RELEASE_DEDUP_SKIPPED_SQL, RELEASE_FALLBACK_PERSISTED_SQL, RELEASE_SUCCESS_SQL,
};

#[derive(Debug, Clone)]
pub struct NewArticle {
    pub content_hash: String,
    pub canonical_link: String,
    pub title: String,
    pub body_text: String,
    pub body_html_artifact_id: Option<i64>,
    pub extractor_strategy: String,
    pub extractor_version: i64,
    pub content_quality: String,
    pub word_count: i64,
    pub origin_feed_entry_id: i64,
}

#[derive(Debug, Clone)]
pub struct ArticleInsertOutcome {
    pub article_id: i64,
    pub newly_created: bool,
}

/// Result of [`ArticleRepository::persist_for_entry`]. Every variant except
/// `LeaseLost` means the article row and the feed entry's state committed
/// together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPersistOutcome {
    /// The entry now points at its own article and is `persisted` /
    /// `fallback_persisted`. `newly_created = false` is a self-hit: the
    /// article with this content hash was created from this same entry by an
    /// earlier attempt (e.g. a version that committed the article before the
    /// entry release and then crashed); it counts as persisted, not as a dup.
    Persisted {
        article_id: i64,
        newly_created: bool,
    },
    /// Another entry's article has this content hash; the entry is
    /// `dedup_skipped` and points at that article.
    DedupSkipped { article_id: i64 },
    /// The owner-guarded entry update matched no row (lease reclaimed by
    /// another run). The transaction was rolled back: no article was created.
    LeaseLost,
}

/// Identifier-only projection: AI content is loaded after the task is claimed.
#[derive(Debug, Clone, FromRow)]
pub struct ArticleAiTaskCandidate {
    pub article_id: i64,
    /// The article is inside the publish window (C9 "fresh" membership).
    /// Always true on the fresh list; on the backlog list only for rows that
    /// already have a task of the identity (reconciliation, R2-B1).
    pub in_window: bool,
}

#[derive(Debug, Clone)]
pub struct BackfillArticleCandidate {
    pub article_id: i64,
    pub state: String,
}

/// Current hash ownership without loading article bodies.
#[derive(Debug, Clone, FromRow)]
pub struct ArticleContentHash {
    pub id: i64,
    pub content_hash: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct ContentHashReindexCandidate {
    pub id: i64,
    pub body_text: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateContentHashOutcome {
    Updated,
    Conflict,
    Unchanged,
}

#[async_trait]
pub trait ArticleRepository: Send + Sync {
    async fn insert_or_get_by_content_hash(
        &self,
        article: &NewArticle,
    ) -> Result<ArticleInsertOutcome, StorageError>;

    /// One transaction for the extract invariant "an article exists ⇔ its
    /// entry advanced": insert-or-get the article by content hash, then move
    /// entry `article.origin_feed_entry_id` (guarded by `lease_owner = owner`)
    /// to `persisted` / `fallback_persisted` (own article, `fallback` chooses)
    /// or `dedup_skipped` (another entry's article, decision `hash_dup`).
    /// If the guard matches no row everything is rolled back (`LeaseLost`).
    ///
    /// Precondition: the caller claimed the entry with `owner`.
    async fn persist_for_entry(
        &self,
        article: &NewArticle,
        owner: &str,
        fallback: bool,
        now: OffsetDateTime,
    ) -> Result<EntryPersistOutcome, StorageError>;

    async fn find_by_id(&self, id: i64) -> Result<Option<Article>, StorageError>;

    /// C9 task_gen backlog share（R2-B1）：按 id 升序 + keyset 游标扫描
    /// `state='persisted'` 中**不属于** fresh 列表的 article——窗口外，或窗口内但
    /// 已有本身份任务行（C3 冲突对账通道）。与 [`Self::list_fresh_for_ai_task_gen`]
    /// 不相交、并集为全部 `persisted`，故 backlog 名额不会被尚无任务的窗口内文章占用。
    async fn list_backlog_for_ai_task_gen(
        &self,
        category_key: &str,
        identity: &crate::NewAiResult,
        window: crate::FreshWindow,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError>;
    /// C9 task_gen fresh share: `persisted` articles of the category inside
    /// the publish window (`window.since <= COALESCE(fe.published_at,
    /// fe.discovered_at) <= window.until`, R2-S1) that have no AI row of `identity` yet, oldest source
    /// time first (they leave the window first), ties by id. The anti-join
    /// keeps rows whose task already exists (and may stay `persisted`, e.g.
    /// `permanent_failed`) out of this list; the id-ordered list
    /// ([`Self::list_backlog_for_ai_task_gen`]) still reconciles them.
    async fn list_fresh_for_ai_task_gen(
        &self,
        category_key: &str,
        identity: &crate::NewAiResult,
        window: crate::FreshWindow,
        batch_size: u32,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError>;
    async fn list_in_window_for_backfill(
        &self,
        date_from: Option<OffsetDateTime>,
        date_to: Option<OffsetDateTime>,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<BackfillArticleCandidate>, StorageError>;
    async fn list_for_content_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<ContentHashReindexCandidate>, StorageError>;
    async fn list_content_hashes(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<ArticleContentHash>, StorageError>;
    async fn update_content_hash(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError>;
    /// Single-row preview of [`Self::update_content_hash`] without writing.
    /// 返回值语义与 update_content_hash 完全一致：
    ///   - `Updated`：`current != new`，且 `new_content_hash` 在 articles
    ///     表中没有冲突行（实际 run 会 UPDATE 这一行）
    ///   - `Unchanged`：`current == new`
    ///   - `Conflict`：`current` 行已被删除，或 `new_content_hash` 已被其他
    ///     行占用（partial unique 会拒）
    ///
    /// Single-row probe used by update_content_hash; this does not simulate
    /// preceding writes in a multi-row dry-run.
    async fn peek_content_hash_outcome(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError>;
}

#[derive(Debug, Clone)]
pub struct ArticleRepo {
    pool: StoragePool,
}

impl ArticleRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    /// W11-P3-C-3：PG 入口；旧 `new(SqlitePool)` thin wrapper 保留兼容。
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }
}

// ── 共享 SQL（跨方言完全等价；EXISTS 已 P1 改 CASE WHEN decode i32） ──

const INSERT_ARTICLE_ON_CONFLICT_SQL: &str = r#"
INSERT INTO articles (
    content_hash, canonical_link, title, body_text, body_html_artifact_id,
    extractor_strategy, extractor_version, content_quality, word_count,
    origin_feed_entry_id, state
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'persisted')
ON CONFLICT(content_hash) DO NOTHING
RETURNING id
"#;

const SELECT_ARTICLE_ID_BY_CONTENT_HASH_SQL: &str =
    "SELECT id FROM articles WHERE content_hash = $1";

const SELECT_ARTICLE_ID_AND_ORIGIN_BY_CONTENT_HASH_SQL: &str =
    "SELECT id, origin_feed_entry_id FROM articles WHERE content_hash = $1";

const SELECT_ARTICLE_BY_ID_SQL: &str = r#"
SELECT id, content_hash, canonical_link, title, body_text,
       body_html_artifact_id, extractor_strategy, extractor_version,
       content_quality, word_count, origin_feed_entry_id, state,
       created_at, updated_at
FROM articles
WHERE id = $1
"#;

/// C9 backlog task_gen list, SQLite (R2-B1): the complement of
/// [`LIST_FRESH_FOR_AI_TASK_GEN_SQLITE_SQL`] within `persisted` — outside the
/// window, or inside it with an identity row already (reconciled here).
/// Window membership as in the fresh list (`$3` / `$8` the SQLite window
/// bounds).
const LIST_BACKLOG_FOR_AI_TASK_GEN_SQLITE_SQL: &str = r#"
SELECT article_id, in_window
FROM (
    SELECT a.id AS article_id,
           (COALESCE(julianday(COALESCE(fe.published_at, fe.discovered_at)) >= julianday($3),
                     COALESCE(fe.published_at, fe.discovered_at) >= $3)
            AND COALESCE(julianday(COALESCE(fe.published_at, fe.discovered_at)) <= julianday($8),
                         COALESCE(fe.published_at, fe.discovered_at) <= $8)) AS in_window,
           EXISTS (
               SELECT 1 FROM article_ai_results r
               WHERE r.article_id = a.id AND r.prompt_version = $4
                 AND r.output_schema_version = $5 AND r.model_id = $6
           ) AS has_identity
    FROM articles a
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE a.state = 'persisted'
      AND a.id > $1
      AND fs.category_key = $2
) candidates
WHERE NOT in_window OR has_identity
ORDER BY article_id ASC
LIMIT $7
"#;

const LIST_BACKLOG_FOR_AI_TASK_GEN_PG_SQL: &str = r#"
SELECT article_id, in_window
FROM (
    SELECT a.id AS article_id,
           (COALESCE(fe.published_at, fe.discovered_at) >= $3
            AND COALESCE(fe.published_at, fe.discovered_at) <= $8) AS in_window,
           EXISTS (
               SELECT 1 FROM article_ai_results r
               WHERE r.article_id = a.id AND r.prompt_version = $4
                 AND r.output_schema_version = $5 AND r.model_id = $6
           ) AS has_identity
    FROM articles a
    JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
    JOIN feed_sources fs ON fs.id = fe.source_id
    WHERE a.state = 'persisted'
      AND a.id > $1
      AND fs.category_key = $2
) candidates
WHERE NOT in_window OR has_identity
ORDER BY article_id ASC
LIMIT $7
"#;

/// C9 fresh task_gen list, SQLite: the source time is compared and ordered
/// by `julianday()` instant with a text fallback; `$2` / `$7` are the window
/// bounds rounded inward to whole milliseconds (`sqlite_window_bounds`).
const LIST_FRESH_FOR_AI_TASK_GEN_SQLITE_SQL: &str = r#"
SELECT a.id AS article_id, 1 AS in_window
FROM articles a
JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
JOIN feed_sources fs ON fs.id = fe.source_id
WHERE a.state = 'persisted'
  AND fs.category_key = $1
  AND COALESCE(julianday(COALESCE(fe.published_at, fe.discovered_at)) >= julianday($2),
               COALESCE(fe.published_at, fe.discovered_at) >= $2)
  AND COALESCE(julianday(COALESCE(fe.published_at, fe.discovered_at)) <= julianday($7),
               COALESCE(fe.published_at, fe.discovered_at) <= $7)
  AND NOT EXISTS (
      SELECT 1 FROM article_ai_results r
      WHERE r.article_id = a.id AND r.prompt_version = $3
        AND r.output_schema_version = $4 AND r.model_id = $5
  )
ORDER BY julianday(COALESCE(fe.published_at, fe.discovered_at)) ASC,
         COALESCE(fe.published_at, fe.discovered_at) ASC,
         a.id ASC
LIMIT $6
"#;

const LIST_FRESH_FOR_AI_TASK_GEN_PG_SQL: &str = r#"
SELECT a.id AS article_id, TRUE AS in_window
FROM articles a
JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
JOIN feed_sources fs ON fs.id = fe.source_id
WHERE a.state = 'persisted'
  AND fs.category_key = $1
  AND COALESCE(fe.published_at, fe.discovered_at) >= $2
  AND COALESCE(fe.published_at, fe.discovered_at) <= $7
  AND NOT EXISTS (
      SELECT 1 FROM article_ai_results r
      WHERE r.article_id = a.id AND r.prompt_version = $3
        AND r.output_schema_version = $4 AND r.model_id = $5
  )
ORDER BY COALESCE(fe.published_at, fe.discovered_at) ASC, a.id ASC
LIMIT $6
"#;

const LIST_ARTICLES_IN_WINDOW_FOR_BACKFILL_SQL: &str = r#"
SELECT id AS article_id, state
FROM articles
WHERE state <> 'retired'
  AND id > $1
  AND ($2 IS NULL OR created_at >= $2)
  AND ($3 IS NULL OR created_at < $3)
ORDER BY id ASC
LIMIT $4
"#;

// SQLite TEXT `created_at`: compare normalised UTC text, see `feed_entry_sql.rs`
// and `super::sqlite_utc_seconds`. PostgreSQL keeps the shared SQL.
const LIST_ARTICLES_IN_WINDOW_FOR_BACKFILL_SQLITE_SQL: &str = r#"
SELECT id AS article_id, state
FROM articles
WHERE state <> 'retired'
  AND id > $1
  AND ($2 IS NULL OR replace(substr(created_at, 1, 19), 'T', ' ') >= $2)
  AND ($3 IS NULL OR replace(substr(created_at, 1, 19), 'T', ' ') < $3)
ORDER BY id ASC
LIMIT $4
"#;

const LIST_ARTICLES_FOR_CONTENT_HASH_REINDEX_SQL: &str = r#"
SELECT id, body_text, content_hash
FROM articles
WHERE id > $1
ORDER BY id ASC
LIMIT $2
"#;

const LIST_ARTICLE_CONTENT_HASHES_SQL: &str = r#"
SELECT id, content_hash
FROM articles
WHERE id > $1
ORDER BY id ASC
LIMIT $2
"#;

pub(super) const UPDATE_ARTICLE_CONTENT_HASH_SQL: &str = r#"
UPDATE articles
SET content_hash = $1, updated_at = $2
WHERE id = $3
"#;

pub(super) const SELECT_ARTICLE_CONTENT_HASH_SQL: &str =
    "SELECT content_hash FROM articles WHERE id = $1";

pub(super) const SELECT_ARTICLE_CONTENT_HASH_COLLISION_SQL: &str = "SELECT CASE WHEN EXISTS(SELECT 1 FROM articles WHERE content_hash = $1 AND id <> $2) THEN 1 ELSE 0 END";

// ── trait 实现：按 backend 分发 ─────────────────────────────────

#[async_trait]
impl ArticleRepository for ArticleRepo {
    async fn insert_or_get_by_content_hash(
        &self,
        article: &NewArticle,
    ) -> Result<ArticleInsertOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_insert_or_get_by_content_hash(p, article).await,
            StoragePool::Postgres(p) => pg_insert_or_get_by_content_hash(p, article).await,
        }
    }

    async fn persist_for_entry(
        &self,
        article: &NewArticle,
        owner: &str,
        fallback: bool,
        now: OffsetDateTime,
    ) -> Result<EntryPersistOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_persist_for_entry(p, article, owner, fallback, now).await
            }
            StoragePool::Postgres(p) => {
                pg_persist_for_entry(p, article, owner, fallback, now).await
            }
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<Article>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_by_id(p, id).await,
            StoragePool::Postgres(p) => pg_find_by_id(p, id).await,
        }
    }

    async fn list_backlog_for_ai_task_gen(
        &self,
        category_key: &str,
        identity: &crate::NewAiResult,
        window: crate::FreshWindow,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                let (since, until) = super::sqlite_window_bounds(window);
                sqlx::query_as::<_, ArticleAiTaskCandidate>(LIST_BACKLOG_FOR_AI_TASK_GEN_SQLITE_SQL)
                    .bind(after_id)
                    .bind(category_key)
                    .bind(since)
                    .bind(identity.prompt_version)
                    .bind(identity.output_schema_version)
                    .bind(&identity.model_id)
                    .bind(i64::from(batch_size))
                    .bind(until)
                    .fetch_all(p)
                    .await
                    .map_err(StorageError::from)
            }
            StoragePool::Postgres(p) => {
                sqlx::query_as::<_, ArticleAiTaskCandidate>(LIST_BACKLOG_FOR_AI_TASK_GEN_PG_SQL)
                    .bind(after_id)
                    .bind(category_key)
                    .bind(window.since)
                    .bind(identity.prompt_version)
                    .bind(identity.output_schema_version)
                    .bind(&identity.model_id)
                    .bind(i64::from(batch_size))
                    .bind(window.until)
                    .fetch_all(p)
                    .await
                    .map_err(StorageError::from)
            }
        }
    }

    async fn list_fresh_for_ai_task_gen(
        &self,
        category_key: &str,
        identity: &crate::NewAiResult,
        window: crate::FreshWindow,
        batch_size: u32,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                let (since, until) = super::sqlite_window_bounds(window);
                sqlx::query_as::<_, ArticleAiTaskCandidate>(LIST_FRESH_FOR_AI_TASK_GEN_SQLITE_SQL)
                    .bind(category_key)
                    .bind(since)
                    .bind(identity.prompt_version)
                    .bind(identity.output_schema_version)
                    .bind(&identity.model_id)
                    .bind(i64::from(batch_size))
                    .bind(until)
                    .fetch_all(p)
                    .await
                    .map_err(StorageError::from)
            }
            StoragePool::Postgres(p) => {
                sqlx::query_as::<_, ArticleAiTaskCandidate>(LIST_FRESH_FOR_AI_TASK_GEN_PG_SQL)
                    .bind(category_key)
                    .bind(window.since)
                    .bind(identity.prompt_version)
                    .bind(identity.output_schema_version)
                    .bind(&identity.model_id)
                    .bind(i64::from(batch_size))
                    .bind(window.until)
                    .fetch_all(p)
                    .await
                    .map_err(StorageError::from)
            }
        }
    }

    async fn list_in_window_for_backfill(
        &self,
        date_from: Option<OffsetDateTime>,
        date_to: Option<OffsetDateTime>,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<BackfillArticleCandidate>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_list_in_window_for_backfill(p, date_from, date_to, batch_size, after_id)
                    .await
            }
            StoragePool::Postgres(p) => {
                pg_list_in_window_for_backfill(p, date_from, date_to, batch_size, after_id).await
            }
        }
    }

    async fn list_for_content_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<ContentHashReindexCandidate>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_list_for_content_hash_reindex(p, after_id, batch_size).await
            }
            StoragePool::Postgres(p) => {
                pg_list_for_content_hash_reindex(p, after_id, batch_size).await
            }
        }
    }

    async fn list_content_hashes(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<ArticleContentHash>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(pool) => sqlx::query_as(LIST_ARTICLE_CONTENT_HASHES_SQL)
                .bind(after_id)
                .bind(i64::from(batch_size))
                .fetch_all(pool)
                .await
                .map_err(StorageError::from),
            StoragePool::Postgres(pool) => sqlx::query_as(LIST_ARTICLE_CONTENT_HASHES_SQL)
                .bind(after_id)
                .bind(i64::from(batch_size))
                .fetch_all(pool)
                .await
                .map_err(StorageError::from),
        }
    }

    async fn update_content_hash(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError> {
        // peek 实现已经走 trait method `match`，update 再 `match` 一次走真正的 UPDATE
        match self.peek_content_hash_outcome(id, new_content_hash).await? {
            UpdateContentHashOutcome::Unchanged => Ok(UpdateContentHashOutcome::Unchanged),
            UpdateContentHashOutcome::Conflict => Ok(UpdateContentHashOutcome::Conflict),
            UpdateContentHashOutcome::Updated => match &self.pool {
                StoragePool::Sqlite(p) => sqlite_update_content_hash(p, id, new_content_hash).await,
                StoragePool::Postgres(p) => pg_update_content_hash(p, id, new_content_hash).await,
            },
        }
    }

    async fn peek_content_hash_outcome(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_peek_content_hash_outcome(p, id, new_content_hash).await
            }
            StoragePool::Postgres(p) => pg_peek_content_hash_outcome(p, id, new_content_hash).await,
        }
    }
}

// ── SQLite helper ──────────────────────────────────────────────

async fn sqlite_insert_or_get_by_content_hash(
    pool: &SqlitePool,
    article: &NewArticle,
) -> Result<ArticleInsertOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let inserted_id = sqlx::query_scalar::<_, i64>(INSERT_ARTICLE_ON_CONFLICT_SQL)
        .bind(&article.content_hash)
        .bind(&article.canonical_link)
        .bind(&article.title)
        .bind(&article.body_text)
        .bind(article.body_html_artifact_id)
        .bind(&article.extractor_strategy)
        .bind(article.extractor_version)
        .bind(&article.content_quality)
        .bind(article.word_count)
        .bind(article.origin_feed_entry_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| classify_db_error(error, "articles", &article.content_hash))?;

    let (article_id, newly_created) = if let Some(id) = inserted_id {
        (id, true)
    } else {
        let id = sqlx::query_scalar::<_, i64>(SELECT_ARTICLE_ID_BY_CONTENT_HASH_SQL)
            .bind(&article.content_hash)
            .fetch_one(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        (id, false)
    };

    tx.commit().await.map_err(StorageError::from)?;
    Ok(ArticleInsertOutcome {
        article_id,
        newly_created,
    })
}

/// How [`ArticleRepository::persist_for_entry`] advances the entry, decided
/// from the insert-or-get result inside the transaction.
enum EntryAdvance {
    Own {
        article_id: i64,
        newly_created: bool,
    },
    Dedup {
        article_id: i64,
    },
}

impl EntryAdvance {
    fn decide(
        inserted_id: Option<i64>,
        existing: Option<(i64, i64)>,
        entry_id: i64,
    ) -> Result<Self, StorageError> {
        match (inserted_id, existing) {
            (Some(article_id), _) => Ok(Self::Own {
                article_id,
                newly_created: true,
            }),
            (None, Some((article_id, origin))) if origin == entry_id => Ok(Self::Own {
                article_id,
                newly_created: false,
            }),
            (None, Some((article_id, _))) => Ok(Self::Dedup { article_id }),
            // ON CONFLICT skipped the insert, so a row with this hash exists in
            // this transaction's view; not finding it is a broken invariant.
            (None, None) => Err(StorageError::Integrity {
                table: "articles".to_string(),
                reference: "content_hash conflict without a visible row".to_string(),
            }),
        }
    }

    fn outcome(&self) -> EntryPersistOutcome {
        match *self {
            Self::Own {
                article_id,
                newly_created,
            } => EntryPersistOutcome::Persisted {
                article_id,
                newly_created,
            },
            Self::Dedup { article_id } => EntryPersistOutcome::DedupSkipped { article_id },
        }
    }
}

async fn sqlite_persist_for_entry(
    pool: &SqlitePool,
    article: &NewArticle,
    owner: &str,
    fallback: bool,
    now: OffsetDateTime,
) -> Result<EntryPersistOutcome, StorageError> {
    let entry_id = article.origin_feed_entry_id;
    // The INSERT is the first statement, so the deferred transaction takes the
    // write lock immediately (no read-then-upgrade busy window).
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let inserted_id = sqlx::query_scalar::<_, i64>(INSERT_ARTICLE_ON_CONFLICT_SQL)
        .bind(&article.content_hash)
        .bind(&article.canonical_link)
        .bind(&article.title)
        .bind(&article.body_text)
        .bind(article.body_html_artifact_id)
        .bind(&article.extractor_strategy)
        .bind(article.extractor_version)
        .bind(&article.content_quality)
        .bind(article.word_count)
        .bind(entry_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| classify_db_error(error, "articles", &article.content_hash))?;
    let existing = match inserted_id {
        Some(_) => None,
        None => sqlx::query_as::<_, (i64, i64)>(SELECT_ARTICLE_ID_AND_ORIGIN_BY_CONTENT_HASH_SQL)
            .bind(&article.content_hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?,
    };
    let advance = EntryAdvance::decide(inserted_id, existing, entry_id)?;
    let updated = match advance {
        EntryAdvance::Own { article_id, .. } => {
            sqlx::query(if fallback {
                RELEASE_FALLBACK_PERSISTED_SQL
            } else {
                RELEASE_SUCCESS_SQL
            })
            .bind(article_id)
            .bind(now)
            .bind(entry_id)
            .bind(owner)
            .execute(&mut *tx)
            .await
        }
        EntryAdvance::Dedup { article_id } => {
            sqlx::query(RELEASE_DEDUP_SKIPPED_SQL)
                .bind("hash_dup")
                .bind(article_id)
                .bind(now)
                .bind(entry_id)
                .bind(owner)
                .execute(&mut *tx)
                .await
        }
    }
    .map_err(StorageError::from)?;
    if updated.rows_affected() != 1 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(EntryPersistOutcome::LeaseLost);
    }
    tx.commit().await.map_err(StorageError::from)?;
    Ok(advance.outcome())
}

async fn sqlite_find_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Article>, StorageError> {
    let row = sqlx::query_as::<_, ArticleRow>(SELECT_ARTICLE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(Article::try_from).transpose()
}

async fn sqlite_list_in_window_for_backfill(
    pool: &SqlitePool,
    date_from: Option<OffsetDateTime>,
    date_to: Option<OffsetDateTime>,
    batch_size: u32,
    after_id: i64,
) -> Result<Vec<BackfillArticleCandidate>, StorageError> {
    sqlx::query_as::<_, BackfillArticleCandidateRow>(
        LIST_ARTICLES_IN_WINDOW_FOR_BACKFILL_SQLITE_SQL,
    )
    .bind(after_id)
    .bind(date_from.map(super::sqlite_utc_seconds))
    .bind(date_to.map(super::sqlite_utc_seconds))
    .bind(i64::from(batch_size))
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(BackfillArticleCandidate::from)
            .collect()
    })
    .map_err(StorageError::from)
}

async fn sqlite_list_for_content_hash_reindex(
    pool: &SqlitePool,
    after_id: i64,
    batch_size: u32,
) -> Result<Vec<ContentHashReindexCandidate>, StorageError> {
    sqlx::query_as::<_, ContentHashReindexCandidate>(LIST_ARTICLES_FOR_CONTENT_HASH_REINDEX_SQL)
        .bind(after_id)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

async fn sqlite_update_content_hash(
    pool: &SqlitePool,
    id: i64,
    new_content_hash: &str,
) -> Result<UpdateContentHashOutcome, StorageError> {
    let result = sqlx::query(UPDATE_ARTICLE_CONTENT_HASH_SQL)
        .bind(new_content_hash)
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    if result.rows_affected() == 1 {
        Ok(UpdateContentHashOutcome::Updated)
    } else {
        Ok(UpdateContentHashOutcome::Conflict)
    }
}

async fn sqlite_peek_content_hash_outcome(
    pool: &SqlitePool,
    id: i64,
    new_content_hash: &str,
) -> Result<UpdateContentHashOutcome, StorageError> {
    let current = sqlx::query_scalar::<_, String>(SELECT_ARTICLE_CONTENT_HASH_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    let Some(current) = current else {
        return Ok(UpdateContentHashOutcome::Conflict);
    };
    if current == new_content_hash {
        return Ok(UpdateContentHashOutcome::Unchanged);
    }

    let conflict = sqlx::query_scalar::<_, i32>(SELECT_ARTICLE_CONTENT_HASH_COLLISION_SQL)
        .bind(new_content_hash)
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?
        != 0;
    if conflict {
        return Ok(UpdateContentHashOutcome::Conflict);
    }
    Ok(UpdateContentHashOutcome::Updated)
}

// ── PostgreSQL helper（W11-P3-C-3） ─────────────────────────────

async fn pg_insert_or_get_by_content_hash(
    pool: &PgPool,
    article: &NewArticle,
) -> Result<ArticleInsertOutcome, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let inserted_id = sqlx::query_scalar::<_, i64>(INSERT_ARTICLE_ON_CONFLICT_SQL)
        .bind(&article.content_hash)
        .bind(&article.canonical_link)
        .bind(&article.title)
        .bind(&article.body_text)
        .bind(article.body_html_artifact_id)
        .bind(&article.extractor_strategy)
        .bind(article.extractor_version)
        .bind(&article.content_quality)
        .bind(article.word_count)
        .bind(article.origin_feed_entry_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| classify_db_error(error, "articles", &article.content_hash))?;

    let (article_id, newly_created) = if let Some(id) = inserted_id {
        (id, true)
    } else {
        let id = sqlx::query_scalar::<_, i64>(SELECT_ARTICLE_ID_BY_CONTENT_HASH_SQL)
            .bind(&article.content_hash)
            .fetch_one(&mut *tx)
            .await
            .map_err(StorageError::from)?;
        (id, false)
    };

    tx.commit().await.map_err(StorageError::from)?;
    Ok(ArticleInsertOutcome {
        article_id,
        newly_created,
    })
}

async fn pg_persist_for_entry(
    pool: &PgPool,
    article: &NewArticle,
    owner: &str,
    fallback: bool,
    now: OffsetDateTime,
) -> Result<EntryPersistOutcome, StorageError> {
    let entry_id = article.origin_feed_entry_id;
    // READ COMMITTED: a concurrent insert of the same hash makes this INSERT
    // wait for it; after its commit ON CONFLICT skips and the SELECT (a new
    // statement snapshot) sees the committed row.
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let inserted_id = sqlx::query_scalar::<_, i64>(INSERT_ARTICLE_ON_CONFLICT_SQL)
        .bind(&article.content_hash)
        .bind(&article.canonical_link)
        .bind(&article.title)
        .bind(&article.body_text)
        .bind(article.body_html_artifact_id)
        .bind(&article.extractor_strategy)
        .bind(article.extractor_version)
        .bind(&article.content_quality)
        .bind(article.word_count)
        .bind(entry_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| classify_db_error(error, "articles", &article.content_hash))?;
    let existing = match inserted_id {
        Some(_) => None,
        None => sqlx::query_as::<_, (i64, i64)>(SELECT_ARTICLE_ID_AND_ORIGIN_BY_CONTENT_HASH_SQL)
            .bind(&article.content_hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?,
    };
    let advance = EntryAdvance::decide(inserted_id, existing, entry_id)?;
    let updated = match advance {
        EntryAdvance::Own { article_id, .. } => {
            sqlx::query(if fallback {
                RELEASE_FALLBACK_PERSISTED_SQL
            } else {
                RELEASE_SUCCESS_SQL
            })
            .bind(article_id)
            .bind(now)
            .bind(entry_id)
            .bind(owner)
            .execute(&mut *tx)
            .await
        }
        EntryAdvance::Dedup { article_id } => {
            sqlx::query(RELEASE_DEDUP_SKIPPED_SQL)
                .bind("hash_dup")
                .bind(article_id)
                .bind(now)
                .bind(entry_id)
                .bind(owner)
                .execute(&mut *tx)
                .await
        }
    }
    .map_err(StorageError::from)?;
    if updated.rows_affected() != 1 {
        tx.rollback().await.map_err(StorageError::from)?;
        return Ok(EntryPersistOutcome::LeaseLost);
    }
    tx.commit().await.map_err(StorageError::from)?;
    Ok(advance.outcome())
}

async fn pg_find_by_id(pool: &PgPool, id: i64) -> Result<Option<Article>, StorageError> {
    let row = sqlx::query_as::<_, ArticleRow>(SELECT_ARTICLE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(Article::try_from).transpose()
}

async fn pg_list_in_window_for_backfill(
    pool: &PgPool,
    date_from: Option<OffsetDateTime>,
    date_to: Option<OffsetDateTime>,
    batch_size: u32,
    after_id: i64,
) -> Result<Vec<BackfillArticleCandidate>, StorageError> {
    sqlx::query_as::<_, BackfillArticleCandidateRow>(LIST_ARTICLES_IN_WINDOW_FOR_BACKFILL_SQL)
        .bind(after_id)
        .bind(date_from)
        .bind(date_to)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(BackfillArticleCandidate::from)
                .collect()
        })
        .map_err(StorageError::from)
}

async fn pg_list_for_content_hash_reindex(
    pool: &PgPool,
    after_id: i64,
    batch_size: u32,
) -> Result<Vec<ContentHashReindexCandidate>, StorageError> {
    sqlx::query_as::<_, ContentHashReindexCandidate>(LIST_ARTICLES_FOR_CONTENT_HASH_REINDEX_SQL)
        .bind(after_id)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
}

async fn pg_update_content_hash(
    pool: &PgPool,
    id: i64,
    new_content_hash: &str,
) -> Result<UpdateContentHashOutcome, StorageError> {
    let result = sqlx::query(UPDATE_ARTICLE_CONTENT_HASH_SQL)
        .bind(new_content_hash)
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await
        .map_err(StorageError::from)?;
    if result.rows_affected() == 1 {
        Ok(UpdateContentHashOutcome::Updated)
    } else {
        Ok(UpdateContentHashOutcome::Conflict)
    }
}

async fn pg_peek_content_hash_outcome(
    pool: &PgPool,
    id: i64,
    new_content_hash: &str,
) -> Result<UpdateContentHashOutcome, StorageError> {
    let current = sqlx::query_scalar::<_, String>(SELECT_ARTICLE_CONTENT_HASH_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    let Some(current) = current else {
        return Ok(UpdateContentHashOutcome::Conflict);
    };
    if current == new_content_hash {
        return Ok(UpdateContentHashOutcome::Unchanged);
    }

    let conflict = sqlx::query_scalar::<_, i32>(SELECT_ARTICLE_CONTENT_HASH_COLLISION_SQL)
        .bind(new_content_hash)
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?
        != 0;
    if conflict {
        return Ok(UpdateContentHashOutcome::Conflict);
    }
    Ok(UpdateContentHashOutcome::Updated)
}

// ── row 类型 + 解析 ─────────────────────────────────────────────

#[derive(Debug, FromRow)]
struct ArticleRow {
    id: i64,
    content_hash: String,
    canonical_link: String,
    title: String,
    body_text: String,
    body_html_artifact_id: Option<i64>,
    extractor_strategy: String,
    extractor_version: i64,
    content_quality: String,
    word_count: i64,
    origin_feed_entry_id: i64,
    state: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct BackfillArticleCandidateRow {
    article_id: i64,
    state: String,
}

impl From<BackfillArticleCandidateRow> for BackfillArticleCandidate {
    fn from(row: BackfillArticleCandidateRow) -> Self {
        Self {
            article_id: row.article_id,
            state: row.state,
        }
    }
}

impl TryFrom<ArticleRow> for Article {
    type Error = StorageError;

    fn try_from(row: ArticleRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            content_hash: row.content_hash,
            canonical_link: row.canonical_link,
            title: row.title,
            body_text: row.body_text,
            body_html_artifact_id: row.body_html_artifact_id,
            extractor_strategy: parse_strategy(&row.extractor_strategy)?,
            extractor_version: row.extractor_version,
            content_quality: parse_quality(&row.content_quality)?,
            word_count: row.word_count,
            origin_feed_entry_id: row.origin_feed_entry_id,
            state: parse_article_state(&row.state)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

fn parse_strategy(value: &str) -> Result<ExtractorStrategy, StorageError> {
    match value {
        "readability" => Ok(ExtractorStrategy::Readability),
        "summary_fallback" => Ok(ExtractorStrategy::SummaryFallback),
        other => Err(StorageError::Corruption(format!(
            "invalid extractor strategy: {other}"
        ))),
    }
}

fn parse_quality(value: &str) -> Result<ContentQuality, StorageError> {
    match value {
        "high" => Ok(ContentQuality::High),
        "medium" => Ok(ContentQuality::Medium),
        "fallback" => Ok(ContentQuality::Fallback),
        other => Err(StorageError::Corruption(format!(
            "invalid content quality: {other}"
        ))),
    }
}

fn parse_article_state(value: &str) -> Result<ArticleState, StorageError> {
    match value {
        "persisted" => Ok(ArticleState::Persisted),
        "ai_pending" => Ok(ArticleState::AiPending),
        "ai_done" => Ok(ArticleState::AiDone),
        "ready_for_publish" => Ok(ArticleState::ReadyForPublish),
        "publish_skipped" => Ok(ArticleState::PublishSkipped),
        "published" => Ok(ArticleState::Published),
        "retired" => Ok(ArticleState::Retired),
        other => Err(StorageError::Corruption(format!(
            "invalid article state: {other}"
        ))),
    }
}
