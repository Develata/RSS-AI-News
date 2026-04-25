use async_trait::async_trait;
use rss_ai_news_config::{ConfigVersionStore, ConfigVersionStoreError};
use sqlx::SqlitePool;

use crate::{StorageError, classify_sqlite_error};

#[async_trait]
pub trait RuleVersionRepository: Send + Sync {
    async fn get_or_create(
        &self,
        kind: &str,
        version_tag: &str,
        description: &str,
        payload_sha256: &str,
    ) -> Result<i64, StorageError>;
}

#[derive(Debug, Clone)]
pub struct SqliteRuleVersionRepo {
    pool: SqlitePool,
}

impl SqliteRuleVersionRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get_or_create_config_version_async(
        &self,
        sha256: &str,
    ) -> Result<i64, StorageError> {
        if let Some(id) = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM rule_versions WHERE kind = 'config' AND payload_sha256 = ? LIMIT 1",
        )
        .bind(sha256)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::from)?
        {
            return Ok(id);
        }

        let version_tag: String = sha256.chars().take(12).collect();
        self.get_or_create("config", &version_tag, "auto-registered config", sha256)
            .await
    }
}

#[async_trait]
impl ConfigVersionStore for SqliteRuleVersionRepo {
    async fn get_or_create_config_version(
        &self,
        sha256: &str,
    ) -> Result<i64, ConfigVersionStoreError> {
        self.get_or_create_config_version_async(sha256)
            .await
            .map_err(|err| ConfigVersionStoreError::Storage(err.to_string()))
    }
}

#[async_trait]
impl RuleVersionRepository for SqliteRuleVersionRepo {
    async fn get_or_create(
        &self,
        kind: &str,
        version_tag: &str,
        description: &str,
        payload_sha256: &str,
    ) -> Result<i64, StorageError> {
        let inserted = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO rule_versions (kind, version_tag, description, payload_sha256)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(kind, version_tag) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(kind)
        .bind(version_tag)
        .bind(description)
        .bind(payload_sha256)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| {
            classify_sqlite_error(error, "rule_versions", format!("{kind}/{version_tag}"))
        })?;

        if let Some(id) = inserted {
            return Ok(id);
        }

        sqlx::query_scalar::<_, i64>(
            "SELECT id FROM rule_versions WHERE kind = ? AND version_tag = ? LIMIT 1",
        )
        .bind(kind)
        .bind(version_tag)
        .fetch_one(&self.pool)
        .await
        .map_err(StorageError::from)
    }
}
