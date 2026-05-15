use sqlx::SqlitePool;

use crate::StorageError;

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), StorageError> {
    sqlx::migrate!("../../migrations/sqlite")
        .run(pool)
        .await
        .map_err(|error| StorageError::Migration(error.to_string()))
}
