use async_trait::async_trait;
use rss_ai_news_domain::model::Article;
use rss_ai_news_domain::state::{ArticleState, ContentQuality, ExtractorStrategy};
use sqlx::{FromRow, PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, StoragePool, classify_db_error};

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

#[derive(Debug, Clone)]
pub struct ArticleAiTaskCandidate {
    pub article_id: i64,
    pub title: String,
    pub body_text: String,
    pub origin_feed_entry_id: i64,
}

#[derive(Debug, Clone)]
pub struct BackfillArticleCandidate {
    pub article_id: i64,
    pub state: String,
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

    async fn find_by_id(&self, id: i64) -> Result<Option<Article>, StorageError>;

    /// task_gen 候选：按 id 升序扫描 `state='persisted'` 的 article。
    async fn list_persisted_for_ai_task_gen(
        &self,
        category_key: &str,
        batch_size: u32,
        after_id: i64,
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
    async fn update_content_hash(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError>;
    /// dry-run 等价：复用 [`Self::update_content_hash`] 的判断逻辑但**不**
    /// 落地任何写。返回值语义与 update_content_hash 完全一致：
    ///   - `Updated`：`current != new`，且 `new_content_hash` 在 articles
    ///     表中没有冲突行（实际 run 会 UPDATE 这一行）
    ///   - `Unchanged`：`current == new`
    ///   - `Conflict`：`current` 行已被删除，或 `new_content_hash` 已被其他
    ///     行占用（partial unique 会拒）
    ///
    /// 仅供 `reindex --dry-run` 使用，让 dry-run 数字可信（cli-semantics
    /// §4.8 line 325 的 "Would update N rows" 含 conflict 区分）。
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

const SELECT_ARTICLE_BY_ID_SQL: &str = r#"
SELECT id, content_hash, canonical_link, title, body_text,
       body_html_artifact_id, extractor_strategy, extractor_version,
       content_quality, word_count, origin_feed_entry_id, state,
       created_at, updated_at
FROM articles
WHERE id = $1
"#;

const LIST_ARTICLES_PERSISTED_FOR_AI_TASK_GEN_SQL: &str = r#"
SELECT a.id AS article_id, a.title, a.body_text, a.origin_feed_entry_id
FROM articles a
JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
JOIN feed_sources fs ON fs.id = fe.source_id
WHERE a.state = 'persisted'
  AND a.id > $1
  AND fs.category_key = $2
ORDER BY a.id ASC
LIMIT $3
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

const LIST_ARTICLES_FOR_CONTENT_HASH_REINDEX_SQL: &str = r#"
SELECT id, body_text, content_hash
FROM articles
WHERE id > $1
ORDER BY id ASC
LIMIT $2
"#;

const UPDATE_ARTICLE_CONTENT_HASH_SQL: &str = r#"
UPDATE articles
SET content_hash = $1, updated_at = $2
WHERE id = $3
"#;

const SELECT_ARTICLE_CONTENT_HASH_SQL: &str = "SELECT content_hash FROM articles WHERE id = $1";

const SELECT_ARTICLE_CONTENT_HASH_COLLISION_SQL: &str = "SELECT CASE WHEN EXISTS(SELECT 1 FROM articles WHERE content_hash = $1 AND id <> $2) THEN 1 ELSE 0 END";

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

    async fn find_by_id(&self, id: i64) -> Result<Option<Article>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_by_id(p, id).await,
            StoragePool::Postgres(p) => pg_find_by_id(p, id).await,
        }
    }

    async fn list_persisted_for_ai_task_gen(
        &self,
        category_key: &str,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_list_persisted_for_ai_task_gen(p, category_key, batch_size, after_id).await
            }
            StoragePool::Postgres(p) => {
                pg_list_persisted_for_ai_task_gen(p, category_key, batch_size, after_id).await
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

async fn sqlite_find_by_id(pool: &SqlitePool, id: i64) -> Result<Option<Article>, StorageError> {
    let row = sqlx::query_as::<_, ArticleRow>(SELECT_ARTICLE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(Article::try_from).transpose()
}

async fn sqlite_list_persisted_for_ai_task_gen(
    pool: &SqlitePool,
    category_key: &str,
    batch_size: u32,
    after_id: i64,
) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
    sqlx::query_as::<_, ArticleAiTaskCandidateRow>(LIST_ARTICLES_PERSISTED_FOR_AI_TASK_GEN_SQL)
        .bind(after_id)
        .bind(category_key)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().map(ArticleAiTaskCandidate::from).collect())
        .map_err(StorageError::from)
}

async fn sqlite_list_in_window_for_backfill(
    pool: &SqlitePool,
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

async fn pg_find_by_id(pool: &PgPool, id: i64) -> Result<Option<Article>, StorageError> {
    let row = sqlx::query_as::<_, ArticleRow>(SELECT_ARTICLE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(Article::try_from).transpose()
}

async fn pg_list_persisted_for_ai_task_gen(
    pool: &PgPool,
    category_key: &str,
    batch_size: u32,
    after_id: i64,
) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
    sqlx::query_as::<_, ArticleAiTaskCandidateRow>(LIST_ARTICLES_PERSISTED_FOR_AI_TASK_GEN_SQL)
        .bind(after_id)
        .bind(category_key)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().map(ArticleAiTaskCandidate::from).collect())
        .map_err(StorageError::from)
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
struct ArticleAiTaskCandidateRow {
    article_id: i64,
    title: String,
    body_text: String,
    origin_feed_entry_id: i64,
}

#[derive(Debug, FromRow)]
struct BackfillArticleCandidateRow {
    article_id: i64,
    state: String,
}

impl From<ArticleAiTaskCandidateRow> for ArticleAiTaskCandidate {
    fn from(row: ArticleAiTaskCandidateRow) -> Self {
        Self {
            article_id: row.article_id,
            title: row.title,
            body_text: row.body_text,
            origin_feed_entry_id: row.origin_feed_entry_id,
        }
    }
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
