use thiserror::Error;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("conflict on {table}: {key}")]
    Conflict { table: String, key: String },
    #[error("integrity violation on {table}: {reference}")]
    Integrity { table: String, reference: String },
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    #[error("operation timed out")]
    Timeout,
    #[error("data corruption detected: {0}")]
    Corruption(String),
    #[error("migration failed: {0}")]
    Migration(String),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

impl StorageError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Unavailable(_) | Self::Timeout => true,
            Self::Sqlx(sqlx::Error::PoolTimedOut) => true,
            Self::Sqlx(sqlx::Error::Io(_)) => true,
            Self::Sqlx(sqlx::Error::Database(db_error)) => {
                let code = db_error.code().map(|code| code.into_owned());
                matches!(
                    code.as_deref(),
                    Some("5") | Some("6") | Some("SQLITE_BUSY") | Some("SQLITE_LOCKED")
                )
            }
            _ => false,
        }
    }
}

pub fn classify_sqlite_error(
    error: sqlx::Error,
    table: impl Into<String>,
    key: impl Into<String>,
) -> StorageError {
    let table = table.into();
    let key = key.into();

    match &error {
        sqlx::Error::Database(db_error) if is_unique_constraint(db_error.as_ref()) => {
            StorageError::Conflict { table, key }
        }
        sqlx::Error::Database(db_error) if is_foreign_key_constraint(db_error.as_ref()) => {
            StorageError::Integrity {
                table,
                reference: key,
            }
        }
        sqlx::Error::PoolTimedOut => StorageError::Timeout,
        sqlx::Error::Io(io_error) => StorageError::Unavailable(io_error.to_string()),
        _ => StorageError::Sqlx(error),
    }
}

fn is_unique_constraint(error: &dyn sqlx::error::DatabaseError) -> bool {
    let message = error.message().to_ascii_lowercase();
    let code = error.code().map(|code| code.into_owned());

    matches!(code.as_deref(), Some("2067") | Some("1555"))
        || message.contains("unique constraint failed")
}

fn is_foreign_key_constraint(error: &dyn sqlx::error::DatabaseError) -> bool {
    let message = error.message().to_ascii_lowercase();
    let code = error.code().map(|code| code.into_owned());

    matches!(code.as_deref(), Some("787")) || message.contains("foreign key constraint failed")
}
