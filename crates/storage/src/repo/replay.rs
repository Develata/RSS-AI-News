//! Read-only projection for `replay --kind html` (plan/10 §4).
//!
//! An `html_payload` artifact is keyed by the feed entry id (see
//! `runtime/src/flows/extract.rs::write_html_artifact`). Replay needs that
//! entry's link (the base URI the live extraction started from) and, for
//! `--diff`, the article the entry points at. It also returns the entry's
//! `title_raw`, which live extraction hands to the strategy as the title
//! fallback (R3-18). The article is reached through
//! `feed_entries.article_id`, which also covers a content-dedup hit whose
//! entry points at an earlier article.

use async_trait::async_trait;
use rss_ai_news_domain::model::RawArtifact;
use sqlx::FromRow;

use crate::{RawArtifactRepo, RawArtifactRepository, StorageError, StoragePool};

/// The article an HTML replay is diffed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayArticleDigest {
    pub id: i64,
    pub title: String,
    pub word_count: i64,
    pub content_hash: String,
}

/// The feed entry an `html_payload` artifact belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlReplaySource {
    pub feed_entry_id: i64,
    pub normalized_link: String,
    /// The feed title live extraction passes as `ArticleFetchTask::title_raw`.
    pub title_raw: String,
    /// `None` while the entry has not produced (or linked) an article.
    pub article: Option<ReplayArticleDigest>,
}

/// Read-only: no method of this trait writes. It is the replay flow's only
/// storage dependency, so its side-effect boundary is visible in its
/// dependency types (as with `RecentFeedEntryRepository`); the artifact reads
/// are offered here instead of the writer `RawArtifactRepository` (R3-20).
#[async_trait]
pub trait ReplayProjectionRepository: Send + Sync {
    /// `raw_artifacts` row by id, of any kind.
    async fn artifact_by_id(&self, id: i64) -> Result<Option<RawArtifact>, StorageError>;

    /// `raw_artifacts` row by `(kind, artifact_key)`.
    async fn artifact_by_key(
        &self,
        kind: &str,
        artifact_key: &str,
    ) -> Result<Option<RawArtifact>, StorageError>;

    async fn html_replay_source(
        &self,
        feed_entry_id: i64,
    ) -> Result<Option<HtmlReplaySource>, StorageError>;
}

#[derive(Debug, Clone)]
pub struct ReplayProjectionRepo {
    pool: StoragePool,
}

impl ReplayProjectionRepo {
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }
}

// Same text for both dialects: positional `$1` is accepted by SQLite and
// PostgreSQL, and every column is BIGINT / TEXT on both (i64 / String).
const HTML_REPLAY_SOURCE_SQL: &str = r#"
SELECT fe.id AS feed_entry_id,
       fe.normalized_link,
       fe.title_raw,
       a.id AS article_id,
       a.title AS article_title,
       a.word_count AS article_word_count,
       a.content_hash AS article_content_hash
FROM feed_entries fe
LEFT JOIN articles a ON a.id = fe.article_id
WHERE fe.id = $1
"#;

#[derive(Debug, FromRow)]
struct HtmlReplaySourceRow {
    feed_entry_id: i64,
    normalized_link: String,
    title_raw: String,
    article_id: Option<i64>,
    article_title: Option<String>,
    article_word_count: Option<i64>,
    article_content_hash: Option<String>,
}

impl From<HtmlReplaySourceRow> for HtmlReplaySource {
    fn from(row: HtmlReplaySourceRow) -> Self {
        // The LEFT JOIN yields all four article columns or none; the NOT NULL
        // constraints on `articles` make a partial row impossible.
        let article = match (
            row.article_id,
            row.article_title,
            row.article_word_count,
            row.article_content_hash,
        ) {
            (Some(id), Some(title), Some(word_count), Some(content_hash)) => {
                Some(ReplayArticleDigest {
                    id,
                    title,
                    word_count,
                    content_hash,
                })
            }
            _ => None,
        };
        Self {
            feed_entry_id: row.feed_entry_id,
            normalized_link: row.normalized_link,
            title_raw: row.title_raw,
            article,
        }
    }
}

#[async_trait]
impl ReplayProjectionRepository for ReplayProjectionRepo {
    // The artifact reads reuse the artifact repository's queries (one SQL
    // source of truth); only its read methods are reachable through here.
    async fn artifact_by_id(&self, id: i64) -> Result<Option<RawArtifact>, StorageError> {
        RawArtifactRepo::new_with_storage(self.pool.clone())
            .find_by_id(id)
            .await
    }

    async fn artifact_by_key(
        &self,
        kind: &str,
        artifact_key: &str,
    ) -> Result<Option<RawArtifact>, StorageError> {
        RawArtifactRepo::new_with_storage(self.pool.clone())
            .find_by_key(kind, artifact_key)
            .await
    }

    async fn html_replay_source(
        &self,
        feed_entry_id: i64,
    ) -> Result<Option<HtmlReplaySource>, StorageError> {
        let row = match &self.pool {
            StoragePool::Sqlite(pool) => {
                sqlx::query_as::<_, HtmlReplaySourceRow>(HTML_REPLAY_SOURCE_SQL)
                    .bind(feed_entry_id)
                    .fetch_optional(pool)
                    .await
            }
            StoragePool::Postgres(pool) => {
                sqlx::query_as::<_, HtmlReplaySourceRow>(HTML_REPLAY_SOURCE_SQL)
                    .bind(feed_entry_id)
                    .fetch_optional(pool)
                    .await
            }
        }
        .map_err(StorageError::from)?;
        Ok(row.map(HtmlReplaySource::from))
    }
}
