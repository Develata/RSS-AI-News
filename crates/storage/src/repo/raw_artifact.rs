use async_trait::async_trait;
use rss_ai_news_domain::{model::RawArtifact, state::ArtifactKind};
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, StoragePool, classify_sqlite_error};

#[derive(Debug, Clone)]
pub struct NewRawArtifact {
    pub kind: String,
    pub artifact_key: String,
    pub content_encoding: String,
    pub inline_body: Vec<u8>,
    pub byte_size: i64,
    pub sha256: String,
    pub retention_policy: String,
    pub expires_at: Option<OffsetDateTime>,
}

#[async_trait]
pub trait RawArtifactRepository: Send + Sync {
    async fn upsert_inline(&self, artifact: &NewRawArtifact) -> Result<i64, StorageError>;
    async fn find_by_key(
        &self,
        kind: &str,
        artifact_key: &str,
    ) -> Result<Option<RawArtifact>, StorageError>;
    async fn find_by_id(&self, id: i64) -> Result<Option<RawArtifact>, StorageError>;
}

#[derive(Debug, Clone)]
pub struct SqliteRawArtifactRepo {
    pool: StoragePool,
}

impl SqliteRawArtifactRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    fn sqlite_pool(&self) -> Result<&SqlitePool, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => Ok(p),
            StoragePool::Postgres(_) => Err(StorageError::UnsupportedBackend(
                "raw_artifact_repo postgres path is P3+".into(),
            )),
        }
    }
}

#[async_trait]
impl RawArtifactRepository for SqliteRawArtifactRepo {
    async fn upsert_inline(&self, artifact: &NewRawArtifact) -> Result<i64, StorageError> {
        let pool = self.sqlite_pool()?;
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO raw_artifacts (
                kind, artifact_key, content_encoding, storage_kind, inline_body, file_path,
                byte_size, sha256, retention_policy, expires_at
            )
            VALUES ($1, $2, $3, 'inline', $4, NULL, $5, $6, $7, $8)
            ON CONFLICT(kind, artifact_key) DO UPDATE SET
                content_encoding = excluded.content_encoding,
                storage_kind = excluded.storage_kind,
                inline_body = excluded.inline_body,
                file_path = excluded.file_path,
                byte_size = excluded.byte_size,
                sha256 = excluded.sha256,
                retention_policy = excluded.retention_policy,
                expires_at = excluded.expires_at
            RETURNING id
            "#,
        )
        .bind(&artifact.kind)
        .bind(&artifact.artifact_key)
        .bind(&artifact.content_encoding)
        .bind(&artifact.inline_body)
        .bind(artifact.byte_size)
        .bind(&artifact.sha256)
        .bind(&artifact.retention_policy)
        .bind(artifact.expires_at)
        .fetch_one(pool)
        .await
        .map_err(|error| {
            classify_sqlite_error(
                error,
                "raw_artifacts",
                format!("{}/{}", artifact.kind, artifact.artifact_key),
            )
        })
    }

    async fn find_by_key(
        &self,
        kind: &str,
        artifact_key: &str,
    ) -> Result<Option<RawArtifact>, StorageError> {
        let pool = self.sqlite_pool()?;
        let row = sqlx::query_as::<_, RawArtifactRow>(
            r#"
            SELECT id, kind, artifact_key, content_encoding, storage_kind, inline_body,
                   file_path, byte_size, sha256, retention_policy, expires_at, created_at
            FROM raw_artifacts
            WHERE kind = $1 AND artifact_key = $2
            "#,
        )
        .bind(kind)
        .bind(artifact_key)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;

        row.map(RawArtifact::try_from).transpose()
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<RawArtifact>, StorageError> {
        let pool = self.sqlite_pool()?;
        let row = sqlx::query_as::<_, RawArtifactRow>(
            r#"
            SELECT id, kind, artifact_key, content_encoding, storage_kind, inline_body,
                   file_path, byte_size, sha256, retention_policy, expires_at, created_at
            FROM raw_artifacts
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;

        row.map(RawArtifact::try_from).transpose()
    }
}

#[derive(Debug, FromRow)]
struct RawArtifactRow {
    id: i64,
    kind: String,
    artifact_key: String,
    content_encoding: String,
    storage_kind: String,
    inline_body: Option<Vec<u8>>,
    file_path: Option<String>,
    byte_size: i64,
    sha256: String,
    retention_policy: String,
    expires_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

impl TryFrom<RawArtifactRow> for RawArtifact {
    type Error = StorageError;

    fn try_from(row: RawArtifactRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            kind: parse_artifact_kind(&row.kind)?,
            artifact_key: row.artifact_key,
            content_encoding: row.content_encoding,
            storage_kind: row.storage_kind,
            inline_body: row.inline_body,
            file_path: row.file_path,
            byte_size: row.byte_size,
            sha256: row.sha256,
            retention_policy: row.retention_policy,
            expires_at: row.expires_at,
            created_at: row.created_at,
        })
    }
}

fn parse_artifact_kind(value: &str) -> Result<ArtifactKind, StorageError> {
    match value {
        "feed_payload" => Ok(ArtifactKind::FeedPayload),
        "html_payload" => Ok(ArtifactKind::HtmlPayload),
        "ai_raw_response" | "ai_response" => Ok(ArtifactKind::AiRawResponse),
        other => Err(StorageError::Corruption(format!(
            "invalid artifact kind: {other}"
        ))),
    }
}
