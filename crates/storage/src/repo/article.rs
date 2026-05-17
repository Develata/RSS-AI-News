use async_trait::async_trait;
use rss_ai_news_domain::model::Article;
use rss_ai_news_domain::state::{ArticleState, ContentQuality, ExtractorStrategy};
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, StoragePool, classify_sqlite_error};

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

    fn sqlite_pool(&self) -> Result<&SqlitePool, StorageError> {
        self.pool.require_sqlite("article_repo")
    }
}

#[async_trait]
impl ArticleRepository for ArticleRepo {
    async fn insert_or_get_by_content_hash(
        &self,
        article: &NewArticle,
    ) -> Result<ArticleInsertOutcome, StorageError> {
        let pool = self.sqlite_pool()?;
        let mut tx = pool.begin().await.map_err(StorageError::from)?;
        let inserted_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO articles (
                content_hash, canonical_link, title, body_text, body_html_artifact_id,
                extractor_strategy, extractor_version, content_quality, word_count,
                origin_feed_entry_id, state
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'persisted')
            ON CONFLICT(content_hash) DO NOTHING
            RETURNING id
            "#,
        )
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
        .map_err(|error| classify_sqlite_error(error, "articles", &article.content_hash))?;

        let (article_id, newly_created) = if let Some(id) = inserted_id {
            (id, true)
        } else {
            let id =
                sqlx::query_scalar::<_, i64>("SELECT id FROM articles WHERE content_hash = $1")
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

    async fn find_by_id(&self, id: i64) -> Result<Option<Article>, StorageError> {
        let pool = self.sqlite_pool()?;
        let row = sqlx::query_as::<_, ArticleRow>(
            r#"
            SELECT id, content_hash, canonical_link, title, body_text,
                   body_html_artifact_id, extractor_strategy, extractor_version,
                   content_quality, word_count, origin_feed_entry_id, state,
                   created_at, updated_at
            FROM articles
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;

        row.map(Article::try_from).transpose()
    }

    async fn list_persisted_for_ai_task_gen(
        &self,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
        let pool = self.sqlite_pool()?;
        sqlx::query_as::<_, ArticleAiTaskCandidateRow>(
            r#"
            SELECT id AS article_id, title, body_text, origin_feed_entry_id
            FROM articles
            WHERE state = 'persisted' AND id > $1
            ORDER BY id ASC
            LIMIT $2
            "#,
        )
        .bind(after_id)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().map(ArticleAiTaskCandidate::from).collect())
        .map_err(StorageError::from)
    }

    async fn list_in_window_for_backfill(
        &self,
        date_from: Option<OffsetDateTime>,
        date_to: Option<OffsetDateTime>,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<BackfillArticleCandidate>, StorageError> {
        let pool = self.sqlite_pool()?;
        sqlx::query_as::<_, BackfillArticleCandidateRow>(
            r#"
            SELECT id AS article_id, state
            FROM articles
            WHERE state <> 'retired'
              AND id > $1
              AND ($2 IS NULL OR created_at >= $2)
              AND ($3 IS NULL OR created_at < $3)
            ORDER BY id ASC
            LIMIT $4
            "#,
        )
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

    async fn list_for_content_hash_reindex(
        &self,
        after_id: i64,
        batch_size: u32,
    ) -> Result<Vec<ContentHashReindexCandidate>, StorageError> {
        let pool = self.sqlite_pool()?;
        sqlx::query_as::<_, ContentHashReindexCandidate>(
            r#"
            SELECT id, body_text, content_hash
            FROM articles
            WHERE id > $1
            ORDER BY id ASC
            LIMIT $2
            "#,
        )
        .bind(after_id)
        .bind(i64::from(batch_size))
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)
    }

    async fn update_content_hash(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError> {
        match self.peek_content_hash_outcome(id, new_content_hash).await? {
            UpdateContentHashOutcome::Unchanged => Ok(UpdateContentHashOutcome::Unchanged),
            UpdateContentHashOutcome::Conflict => Ok(UpdateContentHashOutcome::Conflict),
            UpdateContentHashOutcome::Updated => {
                let pool = self.sqlite_pool()?;
                let result = sqlx::query(
                    r#"
                    UPDATE articles
                    SET content_hash = $1, updated_at = $2
                    WHERE id = $3
                    "#,
                )
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
        }
    }

    async fn peek_content_hash_outcome(
        &self,
        id: i64,
        new_content_hash: &str,
    ) -> Result<UpdateContentHashOutcome, StorageError> {
        let pool = self.sqlite_pool()?;
        let current =
            sqlx::query_scalar::<_, String>("SELECT content_hash FROM articles WHERE id = $1")
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

        let conflict = sqlx::query_scalar::<_, i32>(
            "SELECT CASE WHEN EXISTS(SELECT 1 FROM articles WHERE content_hash = $1 AND id <> $2) THEN 1 ELSE 0 END",
        )
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
}

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
