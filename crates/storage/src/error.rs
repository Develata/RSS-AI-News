use thiserror::Error;

use rss_ai_news_domain::error::ClassifiedError;

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
    #[error("unsupported storage backend: {0}")]
    UnsupportedBackend(String),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

impl StorageError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Unavailable(_) | Self::Timeout => true,
            Self::Sqlx(sqlx::Error::PoolTimedOut) => true,
            Self::Sqlx(sqlx::Error::Io(_)) => true,
            Self::Sqlx(sqlx::Error::Database(db_error)) => is_retryable_db_code(db_error.as_ref()),
            _ => false,
        }
    }
}

impl ClassifiedError for StorageError {
    fn is_retryable(&self) -> bool {
        StorageError::is_retryable(self)
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::Conflict { .. } => "conflict",
            Self::Integrity { .. } => "integrity",
            Self::Unavailable(_) => "db_unavailable",
            Self::Timeout => "db_timeout",
            Self::Corruption(_) => "corruption",
            Self::Migration(_) => "migration",
            Self::UnsupportedBackend(_) => "unsupported_backend",
            Self::Sqlx(error) => match error {
                sqlx::Error::PoolTimedOut => "db_timeout",
                sqlx::Error::Io(_) => "db_unavailable",
                _ => "db_error",
            },
        }
    }

    fn display_user(&self) -> String {
        format!("{self}")
    }

    fn display_debug(&self) -> String {
        format!("{self:?}")
    }
}

/// W11-P3-B：多方言数据库错误分类。
///
/// 按 `sqlx::Error::Database` 的 SQLSTATE / SQLite extended code 分发：
/// - 唯一约束违例（SQLite 2067/1555、PG 23505）→ [`StorageError::Conflict`]
/// - FK / NOT NULL / CHECK 违例（SQLite 787、PG 23503/23502/23514）→ [`StorageError::Integrity`]
/// - 连接 / 序列化 / 死锁等可重试（PG 40001/40P01/08xxx/57P0x）→ [`StorageError::Unavailable`]
///   （让 [`StorageError::is_retryable`] 返回 true）
/// - 其它走 fallthrough [`StorageError::Sqlx`]
///
/// `table` / `key` 进入 Conflict/Integrity 的 message，用于上层定位实体。
/// 设计参考 storage-multi-dialect §6.6。
pub fn classify_db_error(
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
        sqlx::Error::Database(db_error) if is_integrity_violation(db_error.as_ref()) => {
            StorageError::Integrity {
                table,
                reference: key,
            }
        }
        sqlx::Error::Database(db_error) if is_retryable_db_code(db_error.as_ref()) => {
            // 把 PG 40001/40P01 等可重试错误映射成 Unavailable（自动获得
            // is_retryable=true），让上层重试器无需识别 SQLSTATE。
            StorageError::Unavailable(db_error.message().to_string())
        }
        sqlx::Error::PoolTimedOut => StorageError::Timeout,
        sqlx::Error::Io(io_error) => StorageError::Unavailable(io_error.to_string()),
        _ => StorageError::Sqlx(error),
    }
}

fn is_unique_constraint(error: &dyn sqlx::error::DatabaseError) -> bool {
    let code = error.code().map(|code| code.into_owned());
    // SQLite extended codes 2067 (UNIQUE), 1555 (PRIMARY KEY)；
    // PG SQLSTATE 23505 (unique_violation)
    if matches!(code.as_deref(), Some("2067") | Some("1555") | Some("23505")) {
        return true;
    }
    // SQLite 历史 message 兜底（部分老 sqlx 版本 code 缺失）
    let message = error.message().to_ascii_lowercase();
    message.contains("unique constraint failed")
}

fn is_foreign_key_constraint(error: &dyn sqlx::error::DatabaseError) -> bool {
    let code = error.code().map(|code| code.into_owned());
    // SQLite 787 (FOREIGN KEY)；PG SQLSTATE 23503 (foreign_key_violation)
    if matches!(code.as_deref(), Some("787") | Some("23503")) {
        return true;
    }
    let message = error.message().to_ascii_lowercase();
    message.contains("foreign key constraint failed")
}

/// PG NOT NULL (23502) / CHECK (23514) 违例统一映射 Integrity。SQLite 这两类
/// 历史 fallthrough 到 Sqlx；本期不改 SQLite 行为，仅扩展 PG。
fn is_integrity_violation(error: &dyn sqlx::error::DatabaseError) -> bool {
    if is_foreign_key_constraint(error) {
        return true;
    }
    let code = error.code().map(|code| code.into_owned());
    matches!(code.as_deref(), Some("23502") | Some("23514"))
}

/// 可重试 DB 错误码集合：
/// - SQLite: `5` / `6` / `SQLITE_BUSY` / `SQLITE_LOCKED` —— 锁等待，retry 通常解决
/// - PG: `40001` 序列化冲突、`40P01` deadlock、`08000-08007` connection_*、
///   `57P01-57P03` admin_shutdown / cannot_connect_now —— 短暂故障
///
/// 返回 true 时 [`StorageError::is_retryable`] 报告可重试，上层重试器即放行。
fn is_retryable_db_code(error: &dyn sqlx::error::DatabaseError) -> bool {
    let Some(code) = error.code() else {
        return false;
    };
    let code = code.into_owned();
    // SQLite
    if matches!(code.as_str(), "5" | "6" | "SQLITE_BUSY" | "SQLITE_LOCKED") {
        return true;
    }
    // PG SQLSTATE
    matches!(
        code.as_str(),
        // class 40 (transaction rollback)
        "40001" | "40P01"
        // class 08 (connection exception)
        | "08000" | "08001" | "08003" | "08004" | "08006" | "08007"
        // class 57 (operator intervention)
        | "57P01" | "57P02" | "57P03"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::error::{DatabaseError, ErrorKind};
    use std::borrow::Cow;

    /// 测试用 mock DatabaseError，只需对 code / message / kind 给定值。
    /// sqlx 自身 `DatabaseError` 是 trait object；mock 只暴露 P3-B 关心的方法。
    #[derive(Debug)]
    struct MockDb {
        code: Option<&'static str>,
        message: &'static str,
    }

    impl std::fmt::Display for MockDb {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for MockDb {}

    impl DatabaseError for MockDb {
        fn message(&self) -> &str {
            self.message
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            self.code.map(Cow::Borrowed)
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    fn db_err(code: Option<&'static str>, message: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(MockDb { code, message }))
    }

    #[test]
    fn sqlite_unique_2067_maps_to_conflict() {
        let err = classify_db_error(db_err(Some("2067"), "UNIQUE failed"), "feed_sources", "k1");
        assert!(matches!(err, StorageError::Conflict { .. }));
    }

    #[test]
    fn sqlite_pk_1555_maps_to_conflict() {
        let err = classify_db_error(db_err(Some("1555"), "PRIMARY KEY"), "rule_versions", "k");
        assert!(matches!(err, StorageError::Conflict { .. }));
    }

    #[test]
    fn sqlite_fk_787_maps_to_integrity() {
        let err = classify_db_error(db_err(Some("787"), "FOREIGN KEY"), "articles", "fk");
        assert!(matches!(err, StorageError::Integrity { .. }));
    }

    #[test]
    fn sqlite_unique_message_fallback_maps_to_conflict() {
        // 老 sqlx 没填 code 时靠 message 兜底
        let err = classify_db_error(
            db_err(None, "UNIQUE constraint failed: feed_sources.source_key"),
            "feed_sources",
            "k",
        );
        assert!(matches!(err, StorageError::Conflict { .. }));
    }

    #[test]
    fn pg_unique_23505_maps_to_conflict() {
        let err = classify_db_error(
            db_err(Some("23505"), "duplicate key value"),
            "feed_sources",
            "k1",
        );
        assert!(matches!(err, StorageError::Conflict { .. }));
    }

    #[test]
    fn pg_fk_23503_maps_to_integrity() {
        let err = classify_db_error(
            db_err(Some("23503"), "violates foreign key"),
            "articles",
            "fk",
        );
        assert!(matches!(err, StorageError::Integrity { .. }));
    }

    #[test]
    fn pg_not_null_23502_maps_to_integrity() {
        let err = classify_db_error(
            db_err(Some("23502"), "null value in column"),
            "feed_sources",
            "k",
        );
        assert!(matches!(err, StorageError::Integrity { .. }));
    }

    #[test]
    fn pg_check_23514_maps_to_integrity() {
        let err = classify_db_error(
            db_err(Some("23514"), "check constraint"),
            "article_ai_results",
            "k",
        );
        assert!(matches!(err, StorageError::Integrity { .. }));
    }

    #[test]
    fn pg_serialization_40001_is_retryable_unavailable() {
        let err = classify_db_error(
            db_err(Some("40001"), "could not serialize access"),
            "reindex_jobs",
            "k",
        );
        assert!(matches!(err, StorageError::Unavailable(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn pg_deadlock_40p01_is_retryable_unavailable() {
        let err = classify_db_error(
            db_err(Some("40P01"), "deadlock detected"),
            "reindex_jobs",
            "k",
        );
        assert!(matches!(err, StorageError::Unavailable(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn pg_connection_08006_is_retryable_unavailable() {
        let err = classify_db_error(
            db_err(Some("08006"), "connection failure"),
            "feed_sources",
            "k",
        );
        assert!(matches!(err, StorageError::Unavailable(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn pg_admin_shutdown_57p01_is_retryable_unavailable() {
        let err = classify_db_error(db_err(Some("57P01"), "admin shutdown"), "feed_sources", "k");
        assert!(matches!(err, StorageError::Unavailable(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn sqlite_busy_5_is_retryable_via_sqlx_fallthrough() {
        // SQLite busy/locked 不是 unique/integrity/PG retryable code，
        // 但 is_retryable_db_code 会返 true —— 此时 classify_db_error
        // 走"Unavailable retry"分支。
        let err = classify_db_error(db_err(Some("5"), "database is locked"), "x", "k");
        assert!(matches!(err, StorageError::Unavailable(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn unknown_pg_code_falls_through_to_sqlx() {
        let err = classify_db_error(db_err(Some("99999"), "unknown"), "x", "k");
        assert!(matches!(err, StorageError::Sqlx(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn io_error_maps_to_unavailable() {
        let io = std::io::Error::other("network down");
        let err = classify_db_error(sqlx::Error::Io(io), "x", "k");
        assert!(matches!(err, StorageError::Unavailable(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn pool_timed_out_maps_to_timeout() {
        let err = classify_db_error(sqlx::Error::PoolTimedOut, "x", "k");
        assert!(matches!(err, StorageError::Timeout));
        assert!(err.is_retryable());
    }
}
