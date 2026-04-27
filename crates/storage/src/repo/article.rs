use async_trait::async_trait;
use rss_ai_news_domain::model::Article;
use rss_ai_news_domain::state::{ArticleState, ContentQuality, ExtractorStrategy};
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, classify_sqlite_error};

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
}

#[derive(Debug, Clone)]
pub struct SqliteArticleRepo {
    pool: SqlitePool,
}

impl SqliteArticleRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ArticleRepository for SqliteArticleRepo {
    async fn insert_or_get_by_content_hash(
        &self,
        article: &NewArticle,
    ) -> Result<ArticleInsertOutcome, StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::from)?;
        let inserted_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO articles (
                content_hash, canonical_link, title, body_text, body_html_artifact_id,
                extractor_strategy, extractor_version, content_quality, word_count,
                origin_feed_entry_id, state
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'persisted')
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
            let id = sqlx::query_scalar::<_, i64>("SELECT id FROM articles WHERE content_hash = ?")
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
        let row = sqlx::query_as::<_, ArticleRow>(
            r#"
            SELECT id, content_hash, canonical_link, title, body_text,
                   body_html_artifact_id, extractor_strategy, extractor_version,
                   content_quality, word_count, origin_feed_entry_id, state,
                   created_at, updated_at
            FROM articles
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::from)?;

        row.map(Article::try_from).transpose()
    }

    async fn list_persisted_for_ai_task_gen(
        &self,
        batch_size: u32,
        after_id: i64,
    ) -> Result<Vec<ArticleAiTaskCandidate>, StorageError> {
        sqlx::query_as::<_, ArticleAiTaskCandidateRow>(
            r#"
            SELECT id AS article_id, title, body_text, origin_feed_entry_id
            FROM articles
            WHERE state = 'persisted' AND id > ?
            ORDER BY id ASC
            LIMIT ?
            "#,
        )
        .bind(after_id)
        .bind(i64::from(batch_size))
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(ArticleAiTaskCandidate::from).collect())
        .map_err(StorageError::from)
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
        "rule" => Ok(ExtractorStrategy::Rule),
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
