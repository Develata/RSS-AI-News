use async_trait::async_trait;
use rss_ai_news_domain::{
    model::FeedSource,
    state::{FeedKind, FeedSourceStatus},
};
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, classify_sqlite_error};

#[async_trait]
pub trait FeedSourceRepository: Send + Sync {
    async fn upsert(&self, src: &FeedSource) -> Result<i64, StorageError>;
    async fn find_by_id(&self, id: i64) -> Result<Option<FeedSource>, StorageError>;
    async fn find_by_keys(
        &self,
        category_key: &str,
        source_key: &str,
    ) -> Result<Option<FeedSource>, StorageError>;
    async fn list_by_category(&self, category_key: &str) -> Result<Vec<FeedSource>, StorageError>;
}

#[derive(Debug, Clone)]
pub struct SqliteFeedSourceRepo {
    pool: SqlitePool,
}

impl SqliteFeedSourceRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl FeedSourceRepository for SqliteFeedSourceRepo {
    async fn upsert(&self, src: &FeedSource) -> Result<i64, StorageError> {
        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO feed_sources (
                category_key, source_key, display_name, feed_url, feed_kind, status,
                priority, etag, last_modified, last_fetched_at, last_success_at,
                consecutive_failures, last_error, last_error_kind, config_version,
                created_at, updated_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(category_key, source_key) DO UPDATE SET
                display_name = excluded.display_name,
                feed_url = excluded.feed_url,
                feed_kind = excluded.feed_kind,
                status = excluded.status,
                priority = excluded.priority,
                etag = excluded.etag,
                last_modified = excluded.last_modified,
                last_fetched_at = excluded.last_fetched_at,
                last_success_at = excluded.last_success_at,
                consecutive_failures = excluded.consecutive_failures,
                last_error = excluded.last_error,
                last_error_kind = excluded.last_error_kind,
                config_version = excluded.config_version,
                updated_at = excluded.updated_at
            RETURNING id
            "#,
        )
        .bind(&src.category_key)
        .bind(&src.source_key)
        .bind(&src.display_name)
        .bind(&src.feed_url)
        .bind(feed_kind_to_str(src.feed_kind))
        .bind(feed_source_status_to_str(src.status))
        .bind(src.priority)
        .bind(&src.etag)
        .bind(&src.last_modified)
        .bind(src.last_fetched_at)
        .bind(src.last_success_at)
        .bind(src.consecutive_failures)
        .bind(&src.last_error)
        .bind(&src.last_error_kind)
        .bind(src.config_version)
        .bind(src.created_at)
        .bind(src.updated_at)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "feed_sources",
                format!("{}/{}", src.category_key, src.source_key),
            )
        })?;

        Ok(id)
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<FeedSource>, StorageError> {
        let row = sqlx::query_as::<_, FeedSourceRow>(SELECT_FEED_SOURCE_BY_ID)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::from)?;

        row.map(FeedSource::try_from).transpose()
    }

    async fn find_by_keys(
        &self,
        category_key: &str,
        source_key: &str,
    ) -> Result<Option<FeedSource>, StorageError> {
        let row = sqlx::query_as::<_, FeedSourceRow>(SELECT_FEED_SOURCE_BY_KEYS)
            .bind(category_key)
            .bind(source_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(StorageError::from)?;

        row.map(FeedSource::try_from).transpose()
    }

    async fn list_by_category(&self, category_key: &str) -> Result<Vec<FeedSource>, StorageError> {
        let rows = sqlx::query_as::<_, FeedSourceRow>(
            r#"
            SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
                   priority, etag, last_modified, last_fetched_at, last_success_at,
                   consecutive_failures, last_error, last_error_kind, config_version,
                   created_at, updated_at
            FROM feed_sources
            WHERE category_key = ?
            ORDER BY priority ASC, source_key ASC
            "#,
        )
        .bind(category_key)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::from)?;

        rows.into_iter().map(FeedSource::try_from).collect()
    }
}

const SELECT_FEED_SOURCE_BY_ID: &str = r#"
SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
       priority, etag, last_modified, last_fetched_at, last_success_at,
       consecutive_failures, last_error, last_error_kind, config_version,
       created_at, updated_at
FROM feed_sources
WHERE id = ?
"#;

const SELECT_FEED_SOURCE_BY_KEYS: &str = r#"
SELECT id, category_key, source_key, display_name, feed_url, feed_kind, status,
       priority, etag, last_modified, last_fetched_at, last_success_at,
       consecutive_failures, last_error, last_error_kind, config_version,
       created_at, updated_at
FROM feed_sources
WHERE category_key = ? AND source_key = ?
"#;

#[derive(Debug, FromRow)]
struct FeedSourceRow {
    id: i64,
    category_key: String,
    source_key: String,
    display_name: String,
    feed_url: String,
    feed_kind: String,
    status: String,
    priority: i64,
    etag: Option<String>,
    last_modified: Option<String>,
    last_fetched_at: Option<OffsetDateTime>,
    last_success_at: Option<OffsetDateTime>,
    consecutive_failures: i64,
    last_error: Option<String>,
    last_error_kind: Option<String>,
    config_version: i64,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<FeedSourceRow> for FeedSource {
    type Error = StorageError;

    fn try_from(row: FeedSourceRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            category_key: row.category_key,
            source_key: row.source_key,
            display_name: row.display_name,
            feed_url: row.feed_url,
            feed_kind: parse_feed_kind(&row.feed_kind)?,
            status: parse_feed_source_status(&row.status)?,
            priority: row.priority,
            etag: row.etag,
            last_modified: row.last_modified,
            last_fetched_at: row.last_fetched_at,
            last_success_at: row.last_success_at,
            consecutive_failures: row.consecutive_failures,
            last_error: row.last_error,
            last_error_kind: row.last_error_kind,
            config_version: row.config_version,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

fn feed_kind_to_str(kind: FeedKind) -> &'static str {
    match kind {
        FeedKind::Rss => "rss",
        FeedKind::Atom => "atom",
        FeedKind::JsonFeed => "json_feed",
        FeedKind::RssHub => "rss_hub",
    }
}

fn parse_feed_kind(value: &str) -> Result<FeedKind, StorageError> {
    match value {
        "rss" => Ok(FeedKind::Rss),
        "atom" => Ok(FeedKind::Atom),
        "json_feed" => Ok(FeedKind::JsonFeed),
        "rss_hub" | "rsshub" => Ok(FeedKind::RssHub),
        other => Err(StorageError::Corruption(format!(
            "invalid feed_kind: {other}"
        ))),
    }
}

fn feed_source_status_to_str(status: FeedSourceStatus) -> &'static str {
    match status {
        FeedSourceStatus::Active => "active",
        FeedSourceStatus::Paused => "paused",
        FeedSourceStatus::Archived => "archived",
    }
}

fn parse_feed_source_status(value: &str) -> Result<FeedSourceStatus, StorageError> {
    match value {
        "active" => Ok(FeedSourceStatus::Active),
        "paused" => Ok(FeedSourceStatus::Paused),
        "archived" => Ok(FeedSourceStatus::Archived),
        other => Err(StorageError::Corruption(format!(
            "invalid feed source status: {other}"
        ))),
    }
}
