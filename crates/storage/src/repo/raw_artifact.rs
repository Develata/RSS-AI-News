use async_trait::async_trait;
use rss_ai_news_domain::{model::RawArtifact, state::ArtifactKind};
use sqlx::{FromRow, PgPool, SqlitePool};
use std::num::NonZeroU32;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::{StorageError, StoragePool, classify_db_error};

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

    /// Delete at most `batch_size` expired inline artifacts, oldest first.
    /// Uses the start of the current UTC second as a conservative TTL cutoff.
    /// NULL expiry and file-backed rows are retained. Existing foreign keys
    /// clear artifact references without deleting the owning business rows.
    async fn purge_expired(
        &self,
        _now: OffsetDateTime,
        _batch_size: NonZeroU32,
    ) -> Result<u64, StorageError> {
        Err(StorageError::UnsupportedBackend(
            "artifact TTL cleanup is not implemented by this repository".into(),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct RawArtifactRepo {
    pool: StoragePool,
}

impl RawArtifactRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    /// W11-P3-E-1：PG 入口；旧 `new(SqlitePool)` thin wrapper 保留兼容。
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }
}

// ── 共享 SQL（跨方言完全等价；BLOB / BYTEA 由 sqlx 自动桥接 Vec<u8>） ──

const UPSERT_RAW_ARTIFACT_INLINE_SQL: &str = r#"
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
"#;

const SELECT_RAW_ARTIFACT_BY_KEY_SQL: &str = r#"
SELECT id, kind, artifact_key, content_encoding, storage_kind, inline_body,
       file_path, byte_size, sha256, retention_policy, expires_at, created_at
FROM raw_artifacts
WHERE kind = $1 AND artifact_key = $2
"#;

const SELECT_RAW_ARTIFACT_BY_ID_SQL: &str = r#"
SELECT id, kind, artifact_key, content_encoding, storage_kind, inline_body,
       file_path, byte_size, sha256, retention_policy, expires_at, created_at
FROM raw_artifacts
WHERE id = $1
"#;

// Keep the indexed candidate set bounded before validating SQLite timestamps.
// The numeric guard prevents different RFC3339 fractional precisions from
// deleting an artifact early within the cutoff second.
const PURGE_EXPIRED_SQLITE_SQL: &str = r#"
DELETE FROM raw_artifacts
WHERE id IN (
    SELECT id FROM raw_artifacts
    WHERE expires_at < $1 AND storage_kind = 'inline'
    ORDER BY expires_at
    LIMIT $2
)
AND unixepoch(expires_at) < $3
"#;

const PURGE_EXPIRED_POSTGRES_SQL: &str = r#"
WITH candidates AS (
    SELECT id FROM raw_artifacts
    WHERE expires_at < $1 AND storage_kind = 'inline'
    ORDER BY expires_at
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
DELETE FROM raw_artifacts AS artifact
USING candidates
WHERE artifact.id = candidates.id AND artifact.expires_at < $1
"#;

// ── trait 实现 ─────────────────────────────────────────────────

#[async_trait]
impl RawArtifactRepository for RawArtifactRepo {
    async fn upsert_inline(&self, artifact: &NewRawArtifact) -> Result<i64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_upsert_inline(p, artifact).await,
            StoragePool::Postgres(p) => pg_upsert_inline(p, artifact).await,
        }
    }

    async fn find_by_key(
        &self,
        kind: &str,
        artifact_key: &str,
    ) -> Result<Option<RawArtifact>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_by_key(p, kind, artifact_key).await,
            StoragePool::Postgres(p) => pg_find_by_key(p, kind, artifact_key).await,
        }
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<RawArtifact>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_find_by_id(p, id).await,
            StoragePool::Postgres(p) => pg_find_by_id(p, id).await,
        }
    }

    async fn purge_expired(
        &self,
        now: OffsetDateTime,
        batch_size: NonZeroU32,
    ) -> Result<u64, StorageError> {
        let cutoff =
            now.to_offset(UtcOffset::UTC) - Duration::nanoseconds(i64::from(now.nanosecond()));
        let limit = i64::from(batch_size.get());
        match &self.pool {
            StoragePool::Sqlite(pool) => sqlx::query(PURGE_EXPIRED_SQLITE_SQL)
                .bind(cutoff)
                .bind(limit)
                .bind(cutoff.unix_timestamp())
                .execute(pool)
                .await
                .map(|result| result.rows_affected())
                .map_err(StorageError::from),
            StoragePool::Postgres(pool) => {
                // SKIP LOCKED only covers artifact candidates. Foreign-key
                // SET NULL can still wait on a locked article / AI result.
                // Bound that wait and total statement work on the server;
                // transaction-local settings do not leak into the pool.
                let mut tx = pool.begin().await?;
                sqlx::query("SET LOCAL lock_timeout = '1s'")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("SET LOCAL statement_timeout = '5s'")
                    .execute(&mut *tx)
                    .await?;
                let count = sqlx::query(PURGE_EXPIRED_POSTGRES_SQL)
                    .bind(cutoff)
                    .bind(limit)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                tx.commit().await?;
                Ok(count)
            }
        }
    }
}

// ── SQLite helper ──────────────────────────────────────────────

async fn sqlite_upsert_inline(
    pool: &SqlitePool,
    artifact: &NewRawArtifact,
) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(UPSERT_RAW_ARTIFACT_INLINE_SQL)
        .bind(&artifact.kind)
        .bind(&artifact.artifact_key)
        .bind(&artifact.content_encoding)
        .bind(&artifact.inline_body)
        .bind(artifact.byte_size)
        .bind(&artifact.sha256)
        .bind(&artifact.retention_policy)
        .bind(
            artifact
                .expires_at
                .map(|expiry| expiry.to_offset(UtcOffset::UTC)),
        )
        .fetch_one(pool)
        .await
        .map_err(|error| classify_upsert_error(error, artifact))
}

async fn sqlite_find_by_key(
    pool: &SqlitePool,
    kind: &str,
    artifact_key: &str,
) -> Result<Option<RawArtifact>, StorageError> {
    let row = sqlx::query_as::<_, RawArtifactRow>(SELECT_RAW_ARTIFACT_BY_KEY_SQL)
        .bind(kind)
        .bind(artifact_key)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(RawArtifact::try_from).transpose()
}

async fn sqlite_find_by_id(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<RawArtifact>, StorageError> {
    let row = sqlx::query_as::<_, RawArtifactRow>(SELECT_RAW_ARTIFACT_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(RawArtifact::try_from).transpose()
}

// ── PostgreSQL helper（W11-P3-E-1） ─────────────────────────────

async fn pg_upsert_inline(pool: &PgPool, artifact: &NewRawArtifact) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(UPSERT_RAW_ARTIFACT_INLINE_SQL)
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
        .map_err(|error| classify_upsert_error(error, artifact))
}

async fn pg_find_by_key(
    pool: &PgPool,
    kind: &str,
    artifact_key: &str,
) -> Result<Option<RawArtifact>, StorageError> {
    let row = sqlx::query_as::<_, RawArtifactRow>(SELECT_RAW_ARTIFACT_BY_KEY_SQL)
        .bind(kind)
        .bind(artifact_key)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(RawArtifact::try_from).transpose()
}

async fn pg_find_by_id(pool: &PgPool, id: i64) -> Result<Option<RawArtifact>, StorageError> {
    let row = sqlx::query_as::<_, RawArtifactRow>(SELECT_RAW_ARTIFACT_BY_ID_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)?;
    row.map(RawArtifact::try_from).transpose()
}

// ── helper / row ──────────────────────────────────────────────

fn classify_upsert_error(error: sqlx::Error, artifact: &NewRawArtifact) -> StorageError {
    classify_db_error(
        error,
        "raw_artifacts",
        format!("{}/{}", artifact.kind, artifact.artifact_key),
    )
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
