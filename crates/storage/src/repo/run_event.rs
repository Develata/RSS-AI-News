use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::{StorageError, classify_sqlite_error};

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
pub struct SqliteRunEventRepo {
    pool: SqlitePool,
}

impl SqliteRunEventRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RunEventRepository for SqliteRunEventRepo {
    async fn insert(&self, event: &NewRunEvent) -> Result<i64, StorageError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO run_events (
                run_id, trace_id, stage, severity, event_kind,
                target_kind, target_id, message, context_json
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(&event.run_id)
        .bind(&event.trace_id)
        .bind(&event.stage)
        .bind(&event.severity)
        .bind(&event.event_kind)
        .bind(&event.target_kind)
        .bind(event.target_id)
        .bind(&event.message)
        .bind(&event.context_json)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| classify_sqlite_error(error, "run_events", &event.run_id))
    }
}
