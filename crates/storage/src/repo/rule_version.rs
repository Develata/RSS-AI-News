use async_trait::async_trait;
use rss_ai_news_config::{ConfigVersionStore, ConfigVersionStoreError};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use crate::{StorageError, classify_sqlite_error};

/// 单条 `rule_versions` 行的领域投影。所有规则消费方（ingest / extract /
/// ai_run / publish / rebuild）通过 [`RuleVersionRepository::active_rule`]
/// 获取当前生效版本，禁止直接 `SELECT FROM rule_versions WHERE id = ?`
/// （W9-F4：active_rule resolver 是规则切换的唯一入口）。
#[derive(Debug, Clone)]
pub struct RuleVersion {
    pub id: i64,
    pub kind: String,
    pub version_tag: String,
    pub description: String,
    pub payload_sha256: String,
    pub status: String,
    pub retired_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

#[async_trait]
pub trait RuleVersionRepository: Send + Sync {
    async fn get_or_create(
        &self,
        kind: &str,
        version_tag: &str,
        description: &str,
        payload_sha256: &str,
    ) -> Result<i64, StorageError>;

    /// 取该 `kind` 下当前 `status='active'` 的唯一一行。`None` = 该 kind
    /// 尚无 active 行（首版 migration 未植入对应 kind / 全部行被
    /// `superseded`）。partial unique index `uq_rule_versions_kind_active`
    /// 保证至多返回一行；多 active 在 DB 层就被拒绝写入。
    async fn active_rule(&self, kind: &str) -> Result<Option<RuleVersion>, StorageError>;

    /// 以 `status='pending'` 插入一行（**不**冲突 partial unique
    /// `uq_rule_versions_kind_active`）。reindex 启动时的入口；完成后由
    /// 终止事务统一推进到 `active` 并把旧 active 行写 `superseded`。在
    /// F15-7 两阶段激活完整实现前，调用方手动管理后续状态迁移。
    async fn insert_pending_rule(
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
        // F15-1 引入了 partial unique index `uq_rule_versions_kind_active`
        // (kind WHERE status='active')。get_or_create 不感知"是否首版"，因此
        // 用 CASE/EXISTS 子查询自动选 status：当该 kind 已有 active 行则
        // 新行写 'pending'（避免 partial unique 冲突），否则写 'active'。
        // 这保留 `get_or_create` 对 caller 透明的语义；rule 真正切换由
        // 后续 reindex 终止事务（F15-7）显式 pending → active 推进完成。
        let inserted = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status)
            VALUES (
                ?, ?, ?, ?,
                CASE
                    WHEN EXISTS (
                        SELECT 1 FROM rule_versions
                        WHERE kind = ? AND status = 'active'
                    ) THEN 'pending'
                    ELSE 'active'
                END
            )
            ON CONFLICT(kind, version_tag) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(kind)
        .bind(version_tag)
        .bind(description)
        .bind(payload_sha256)
        .bind(kind)
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

    async fn insert_pending_rule(
        &self,
        kind: &str,
        version_tag: &str,
        description: &str,
        payload_sha256: &str,
    ) -> Result<i64, StorageError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status)
            VALUES (?, ?, ?, ?, 'pending')
            RETURNING id
            "#,
        )
        .bind(kind)
        .bind(version_tag)
        .bind(description)
        .bind(payload_sha256)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            classify_sqlite_error(error, "rule_versions", format!("{kind}/{version_tag}"))
        })
    }

    async fn active_rule(&self, kind: &str) -> Result<Option<RuleVersion>, StorageError> {
        sqlx::query_as::<
            _,
            (
                i64,
                String,
                String,
                String,
                String,
                String,
                Option<OffsetDateTime>,
                OffsetDateTime,
            ),
        >(
            r#"
            SELECT id, kind, version_tag, description, payload_sha256,
                   status, retired_at, created_at
            FROM rule_versions
            WHERE kind = ? AND status = 'active'
            LIMIT 1
            "#,
        )
        .bind(kind)
        .fetch_optional(&self.pool)
        .await
        .map_err(StorageError::from)
        .map(|opt| {
            opt.map(
                |(
                    id,
                    kind,
                    version_tag,
                    description,
                    payload_sha256,
                    status,
                    retired_at,
                    created_at,
                )| RuleVersion {
                    id,
                    kind,
                    version_tag,
                    description,
                    payload_sha256,
                    status,
                    retired_at,
                    created_at,
                },
            )
        })
    }
}
