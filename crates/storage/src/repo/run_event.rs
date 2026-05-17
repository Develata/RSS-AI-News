use async_trait::async_trait;
use sqlx::{PgPool, SqlitePool};

use crate::{StorageError, StoragePool, classify_db_error};

#[derive(Debug, Clone)]
pub struct NewRunEvent {
    pub run_id: String,
    pub trace_id: Option<String>,
    pub stage: String,
    pub severity: String,
    pub event_kind: String,
    pub target_kind: Option<String>,
    pub target_id: Option<i64>,
    pub message: String,
    pub context_json: Option<String>,
}

#[async_trait]
pub trait RunEventRepository: Send + Sync {
    async fn insert(&self, event: &NewRunEvent) -> Result<i64, StorageError>;
}

#[derive(Debug, Clone)]
pub struct RunEventRepo {
    pool: StoragePool,
}

impl RunEventRepo {
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

const INSERT_RUN_EVENT_SQL: &str = r#"
INSERT INTO run_events (
    run_id, trace_id, stage, severity, event_kind,
    target_kind, target_id, message, context_json
)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
RETURNING id
"#;

#[async_trait]
impl RunEventRepository for RunEventRepo {
    async fn insert(&self, event: &NewRunEvent) -> Result<i64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(p) => sqlite_insert(p, event).await,
            StoragePool::Postgres(p) => pg_insert(p, event).await,
        }
    }
}

async fn sqlite_insert(pool: &SqlitePool, event: &NewRunEvent) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_RUN_EVENT_SQL)
        .bind(&event.run_id)
        .bind(&event.trace_id)
        .bind(&event.stage)
        .bind(&event.severity)
        .bind(&event.event_kind)
        .bind(&event.target_kind)
        .bind(event.target_id)
        .bind(&event.message)
        .bind(&event.context_json)
        .fetch_one(pool)
        .await
        .map_err(|error| classify_db_error(error, "run_events", &event.run_id))
}

async fn pg_insert(pool: &PgPool, event: &NewRunEvent) -> Result<i64, StorageError> {
    sqlx::query_scalar::<_, i64>(INSERT_RUN_EVENT_SQL)
        .bind(&event.run_id)
        .bind(&event.trace_id)
        .bind(&event.stage)
        .bind(&event.severity)
        .bind(&event.event_kind)
        .bind(&event.target_kind)
        .bind(event.target_id)
        .bind(&event.message)
        .bind(&event.context_json)
        .fetch_one(pool)
        .await
        .map_err(|error| classify_db_error(error, "run_events", &event.run_id))
}
