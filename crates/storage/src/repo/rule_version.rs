use async_trait::async_trait;
use rss_ai_news_config::{ConfigVersionStore, ConfigVersionStoreError};
use sqlx::{PgPool, SqlitePool};
use time::OffsetDateTime;

use crate::{StorageError, StoragePool, classify_db_error};

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

    /// 生产代码读取"当前生效规则版本"的统一入口（F15-3）：先用
    /// [`active_rule`] 查 `status='active'` 的行，若不存在则用
    /// [`get_or_create`] 以 `default_*` 参数 seed 一个首版（因当前 kind
    /// 无 active 行，`get_or_create` 的 CASE/EXISTS 自动写 'active'）。
    ///
    /// 该方法是 ingest / extract / ai_run / publish / rebuild_report 等
    /// 消费方拿到 `rule_version_id` 的唯一入口；reindex flow 由
    /// `insert_pending_rule` 单独负责，不走此 helper。
    async fn active_rule_or_register(
        &self,
        kind: &str,
        default_version_tag: &str,
        default_description: &str,
        default_payload_sha256: &str,
    ) -> Result<i64, StorageError> {
        if let Some(active) = self.active_rule(kind).await? {
            return Ok(active.id);
        }
        self.get_or_create(
            kind,
            default_version_tag,
            default_description,
            default_payload_sha256,
        )
        .await
    }
}

/// W16（docs/plan/16-config-versioning.md §4）：`kind='config'` 行 sha-keyed
/// 轮换的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigRotation {
    /// active 行的 `payload_sha256` 已与当前 config 一致，未发生任何写入。
    NoChange { id: i64 },
    /// 发生轮换：`new_id` 是轮换后的 active 行；`demoted_id` 是被推到
    /// `superseded` 的旧 active 行（轮换前无 active 行时为 `None`）。
    Rotated {
        new_id: i64,
        demoted_id: Option<i64>,
    },
}

impl ConfigRotation {
    /// 轮换后当前 active config 行的 id。
    pub fn active_id(&self) -> i64 {
        match self {
            Self::NoChange { id } => *id,
            Self::Rotated { new_id, .. } => *new_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuleVersionRepo {
    pool: StoragePool,
}

impl RuleVersionRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool: StoragePool::Sqlite(pool),
        }
    }

    /// W11-P3-E-1：PG 入口；旧 `new(SqlitePool)` thin wrapper 保留兼容。
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }

    pub async fn get_or_create_config_version_async(
        &self,
        sha256: &str,
    ) -> Result<i64, StorageError> {
        // 先查已有的 config sha256：跨方言等价 const SQL。
        let existing = match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlx::query_scalar::<_, i64>(SELECT_CONFIG_VERSION_BY_SHA_SQL)
                    .bind(sha256)
                    .fetch_optional(p)
                    .await
                    .map_err(StorageError::from)?
            }
            StoragePool::Postgres(p) => {
                sqlx::query_scalar::<_, i64>(SELECT_CONFIG_VERSION_BY_SHA_SQL)
                    .bind(sha256)
                    .fetch_optional(p)
                    .await
                    .map_err(StorageError::from)?
            }
        };
        if let Some(id) = existing {
            return Ok(id);
        }
        let version_tag: String = sha256.chars().take(12).collect();
        self.get_or_create("config", &version_tag, "auto-registered config", sha256)
            .await
    }

    /// W16（docs/plan/16-config-versioning.md §4）：让 `kind='config'` 的
    /// active 行跟随当前真实 `config_sha256`。
    ///
    /// 1. active 行的 payload 已等于 `sha256` → [`ConfigRotation::NoChange`]
    ///    （热路径，单 SELECT 零写入）。
    /// 2. 否则单事务内：demote 现 active（若有）→ 按 `payload_sha256` 查既有
    ///    行（pending / superseded 均复活为 active 并清 `retired_at`；config
    ///    回滚 A→B→A 必须复用原行——`(kind, version_tag)` 唯一且 tag 由 sha
    ///    派生，插同 sha 新行走不通）→ 无既有行则插入新 active 行。
    ///
    /// 不变量：事务提交后 `kind='config'` 恰有一行 active 且 payload 等于
    /// `sha256`；partial unique `uq_rule_versions_kind_active` 在 DB 层兜底。
    /// 并发轮换 last-writer-wins（最近启动的 config 生效，语义正确）。
    pub async fn rotate_active_config(
        &self,
        sha256: &str,
        description: &str,
        now: OffsetDateTime,
    ) -> Result<ConfigRotation, StorageError> {
        if let Some(active) = self.active_rule("config").await?
            && active.payload_sha256 == sha256
        {
            return Ok(ConfigRotation::NoChange { id: active.id });
        }
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_rotate_active_config(p, sha256, description, now).await
            }
            StoragePool::Postgres(p) => pg_rotate_active_config(p, sha256, description, now).await,
        }
    }
}

#[async_trait]
impl ConfigVersionStore for RuleVersionRepo {
    async fn get_or_create_config_version(
        &self,
        sha256: &str,
    ) -> Result<i64, ConfigVersionStoreError> {
        self.get_or_create_config_version_async(sha256)
            .await
            .map_err(|err| ConfigVersionStoreError::Storage(err.to_string()))
    }
}

// ── 共享 SQL（跨方言完全等价） ─────────────────────────────────

const SELECT_CONFIG_VERSION_BY_SHA_SQL: &str =
    "SELECT id FROM rule_versions WHERE kind = 'config' AND payload_sha256 = $1 LIMIT 1";

// ── W16 rotate_active_config 三步 SQL（跨方言等价）────────────────
//
// demote 不排除目标 sha：极端并发下另一进程已把同 sha 推 active 时，
// 本事务先 demote 再按 sha 复活同一行，净效果幂等（设计 §4）。

const DEMOTE_ACTIVE_CONFIG_SQL: &str = r#"
UPDATE rule_versions
SET status = 'superseded',
    retired_at = $1
WHERE kind = 'config' AND status = 'active'
RETURNING id
"#;

/// 复活既有行（pending 或 superseded → active）。仅 `kind='config'` 开放
/// superseded→active 迁移（config 回滚 A→B→A），见 16-config-versioning §4。
const REVIVE_CONFIG_VERSION_SQL: &str = r#"
UPDATE rule_versions
SET status = 'active',
    retired_at = NULL
WHERE id = $1 AND kind = 'config'
"#;

const INSERT_ACTIVE_CONFIG_VERSION_SQL: &str = r#"
INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status)
VALUES ('config', $1, $2, $3, 'active')
RETURNING id
"#;

/// F15-1 引入了 partial unique index `uq_rule_versions_kind_active`
/// (kind WHERE status='active')。get_or_create 不感知"是否首版"，因此
/// 用 CASE/EXISTS 子查询自动选 status：该 kind 已有 active 行则新行写
/// 'pending'（避免 partial unique 冲突），否则写 'active'。
const GET_OR_CREATE_RULE_VERSION_SQL: &str = r#"
INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status)
VALUES (
    $1, $2, $3, $4,
    CASE
        WHEN EXISTS (
            SELECT 1 FROM rule_versions
            WHERE kind = $5 AND status = 'active'
        ) THEN 'pending'
        ELSE 'active'
    END
)
ON CONFLICT(kind, version_tag) DO NOTHING
RETURNING id
"#;

const SELECT_RULE_VERSION_BY_KIND_TAG_SQL: &str =
    "SELECT id FROM rule_versions WHERE kind = $1 AND version_tag = $2 LIMIT 1";

const INSERT_PENDING_RULE_VERSION_SQL: &str = r#"
INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status)
VALUES ($1, $2, $3, $4, 'pending')
RETURNING id
"#;

const SELECT_ACTIVE_RULE_BY_KIND_SQL: &str = r#"
SELECT id, kind, version_tag, description, payload_sha256,
       status, retired_at, created_at
FROM rule_versions
WHERE kind = $1 AND status = 'active'
LIMIT 1
"#;

// ── trait 实现：按 backend 分发 ─────────────────────────────────

#[async_trait]
impl RuleVersionRepository for RuleVersionRepo {
    async fn get_or_create(
        &self,
        kind: &str,
        version_tag: &str,
        description: &str,
        payload_sha256: &str,
    ) -> Result<i64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_get_or_create(p, kind, version_tag, description, payload_sha256).await
            }
            StoragePool::Postgres(p) => {
                pg_get_or_create(p, kind, version_tag, description, payload_sha256).await
            }
        }
    }

    async fn insert_pending_rule(
        &self,
        kind: &str,
        version_tag: &str,
        description: &str,
        payload_sha256: &str,
    ) -> Result<i64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => {
                sqlite_insert_pending_rule(p, kind, version_tag, description, payload_sha256).await
            }
            StoragePool::Postgres(p) => {
                pg_insert_pending_rule(p, kind, version_tag, description, payload_sha256).await
            }
        }
    }

    async fn active_rule(&self, kind: &str) -> Result<Option<RuleVersion>, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_active_rule(p, kind).await,
            StoragePool::Postgres(p) => pg_active_rule(p, kind).await,
        }
    }
}

// ── SQLite helper ──────────────────────────────────────────────

async fn sqlite_get_or_create(
    pool: &SqlitePool,
    kind: &str,
    version_tag: &str,
    description: &str,
    payload_sha256: &str,
) -> Result<i64, StorageError> {
    let inserted = sqlx::query_scalar::<_, i64>(GET_OR_CREATE_RULE_VERSION_SQL)
        .bind(kind)
        .bind(version_tag)
        .bind(description)
        .bind(payload_sha256)
        .bind(kind)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            classify_db_error(error, "rule_versions", format!("{kind}/{version_tag}"))
        })?;
    if let Some(id) = inserted {
        return Ok(id);
    }
    sqlx::query_scalar::<_, i64>(SELECT_RULE_VERSION_BY_KIND_TAG_SQL)
        .bind(kind)
        .bind(version_tag)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)
}

async fn sqlite_insert_pending_rule(
    pool: &SqlitePool,
    kind: &str,
    version_tag: &str,
    description: &str,
    payload_sha256: &str,
) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_PENDING_RULE_VERSION_SQL)
        .bind(kind)
        .bind(version_tag)
        .bind(description)
        .bind(payload_sha256)
        .fetch_one(pool)
        .await
        .map_err(|error| classify_db_error(error, "rule_versions", format!("{kind}/{version_tag}")))
}

async fn sqlite_active_rule(
    pool: &SqlitePool,
    kind: &str,
) -> Result<Option<RuleVersion>, StorageError> {
    sqlx::query_as::<_, RuleVersionTuple>(SELECT_ACTIVE_RULE_BY_KIND_SQL)
        .bind(kind)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
        .map(|opt| opt.map(RuleVersion::from))
}

/// W16：demote → 按 sha 复活/插入 → commit，单事务保证"恰一行 active"
/// 不变量没有可观察的中间窗口（SQLite 写串行化下天然成立）。
async fn sqlite_rotate_active_config(
    pool: &SqlitePool,
    sha256: &str,
    description: &str,
    now: OffsetDateTime,
) -> Result<ConfigRotation, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let demoted_id = sqlx::query_scalar::<_, i64>(DEMOTE_ACTIVE_CONFIG_SQL)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    let existing = sqlx::query_scalar::<_, i64>(SELECT_CONFIG_VERSION_BY_SHA_SQL)
        .bind(sha256)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    let new_id = match existing {
        Some(id) => {
            sqlx::query(REVIVE_CONFIG_VERSION_SQL)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::from)?;
            id
        }
        None => {
            let tag: String = sha256.chars().take(12).collect();
            sqlx::query_scalar::<_, i64>(INSERT_ACTIVE_CONFIG_VERSION_SQL)
                .bind(&tag)
                .bind(description)
                .bind(sha256)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| {
                    classify_db_error(error, "rule_versions", format!("config/{tag}"))
                })?
        }
    };
    tx.commit().await.map_err(StorageError::from)?;
    Ok(ConfigRotation::Rotated { new_id, demoted_id })
}

// ── PostgreSQL helper（W11-P3-E-1） ─────────────────────────────

/// codex P3-E-fix1 HIGH-1 修复：PG 上 `get_or_create` 的并发首版 seed race。
///
/// 场景：两连接同 `kind`、**不同** `version_tag` 同时调 `get_or_create`，
/// 二者 CASE/EXISTS 子查询都看到"无 active"，都尝试插 `status='active'` →
/// partial unique `uq_rule_versions_kind_active` (kind WHERE status='active')
/// 让其中一个 INSERT 报 23505，但 `ON CONFLICT(kind, version_tag) DO NOTHING`
/// 不命中（不同 tag），整段 INSERT fail。SQLite 写串行化下不发生此 race。
///
/// 修复：检测到 `StorageError::Conflict { table: "rule_versions", .. }`（23505
/// 映射），retry 一次。重试时 CASE 子查询已能看到另一连接 commit 的 active
/// 行，新插入自动选 `status='pending'`，绕开 partial unique。最坏两 worker
/// 并发，一次重试足够；多重 race 极端情况下重试失败仍向上抛 Conflict 让 CLI
/// 显式 fail（而非无限循环）。
async fn pg_get_or_create(
    pool: &PgPool,
    kind: &str,
    version_tag: &str,
    description: &str,
    payload_sha256: &str,
) -> Result<i64, StorageError> {
    match pg_get_or_create_once(pool, kind, version_tag, description, payload_sha256).await {
        Ok(id) => Ok(id),
        Err(StorageError::Conflict { table, .. }) if table == "rule_versions" => {
            pg_get_or_create_once(pool, kind, version_tag, description, payload_sha256).await
        }
        Err(e) => Err(e),
    }
}

async fn pg_get_or_create_once(
    pool: &PgPool,
    kind: &str,
    version_tag: &str,
    description: &str,
    payload_sha256: &str,
) -> Result<i64, StorageError> {
    let inserted = sqlx::query_scalar::<_, i64>(GET_OR_CREATE_RULE_VERSION_SQL)
        .bind(kind)
        .bind(version_tag)
        .bind(description)
        .bind(payload_sha256)
        .bind(kind)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            classify_db_error(error, "rule_versions", format!("{kind}/{version_tag}"))
        })?;
    if let Some(id) = inserted {
        return Ok(id);
    }
    sqlx::query_scalar::<_, i64>(SELECT_RULE_VERSION_BY_KIND_TAG_SQL)
        .bind(kind)
        .bind(version_tag)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)
}

async fn pg_insert_pending_rule(
    pool: &PgPool,
    kind: &str,
    version_tag: &str,
    description: &str,
    payload_sha256: &str,
) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_PENDING_RULE_VERSION_SQL)
        .bind(kind)
        .bind(version_tag)
        .bind(description)
        .bind(payload_sha256)
        .fetch_one(pool)
        .await
        .map_err(|error| classify_db_error(error, "rule_versions", format!("{kind}/{version_tag}")))
}

/// W16：与 sqlite 同步骤。跨进程并发轮换时 `(kind, version_tag)` 唯一或
/// partial unique 触发 23505 → 单次 retry（沿用 [`pg_get_or_create`] 模式）：
/// 重试时 SELECT BY SHA 已能看到对方 commit 的行，走复活分支。两个不同 sha
/// 共享前 12 hex 的 tag 碰撞（48-bit，概率可忽略）重试后仍 Conflict，
/// 向上显式 fail 不静默。
async fn pg_rotate_active_config(
    pool: &PgPool,
    sha256: &str,
    description: &str,
    now: OffsetDateTime,
) -> Result<ConfigRotation, StorageError> {
    match pg_rotate_active_config_once(pool, sha256, description, now).await {
        Err(StorageError::Conflict { table, .. }) if table == "rule_versions" => {
            pg_rotate_active_config_once(pool, sha256, description, now).await
        }
        other => other,
    }
}

async fn pg_rotate_active_config_once(
    pool: &PgPool,
    sha256: &str,
    description: &str,
    now: OffsetDateTime,
) -> Result<ConfigRotation, StorageError> {
    let mut tx = pool.begin().await.map_err(StorageError::from)?;
    let demoted_id = sqlx::query_scalar::<_, i64>(DEMOTE_ACTIVE_CONFIG_SQL)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    let existing = sqlx::query_scalar::<_, i64>(SELECT_CONFIG_VERSION_BY_SHA_SQL)
        .bind(sha256)
        .fetch_optional(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    let new_id = match existing {
        Some(id) => {
            sqlx::query(REVIVE_CONFIG_VERSION_SQL)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(StorageError::from)?;
            id
        }
        None => {
            let tag: String = sha256.chars().take(12).collect();
            sqlx::query_scalar::<_, i64>(INSERT_ACTIVE_CONFIG_VERSION_SQL)
                .bind(&tag)
                .bind(description)
                .bind(sha256)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| {
                    classify_db_error(error, "rule_versions", format!("config/{tag}"))
                })?
        }
    };
    tx.commit().await.map_err(StorageError::from)?;
    Ok(ConfigRotation::Rotated { new_id, demoted_id })
}

async fn pg_active_rule(pool: &PgPool, kind: &str) -> Result<Option<RuleVersion>, StorageError> {
    sqlx::query_as::<_, RuleVersionTuple>(SELECT_ACTIVE_RULE_BY_KIND_SQL)
        .bind(kind)
        .fetch_optional(pool)
        .await
        .map_err(StorageError::from)
        .map(|opt| opt.map(RuleVersion::from))
}

// ── row tuple + 转换 ──────────────────────────────────────────

type RuleVersionTuple = (
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<OffsetDateTime>,
    OffsetDateTime,
);

impl From<RuleVersionTuple> for RuleVersion {
    fn from(t: RuleVersionTuple) -> Self {
        let (id, kind, version_tag, description, payload_sha256, status, retired_at, created_at) =
            t;
        RuleVersion {
            id,
            kind,
            version_tag,
            description,
            payload_sha256,
            status,
            retired_at,
            created_at,
        }
    }
}
