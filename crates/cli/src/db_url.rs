//! W11-P3-A-fix1.H1：按 storage-multi-dialect §5.4 解析 storage URL。
//!
//! `AppConfig.database.driver` 与 `env.database_url` 同时存在，启动期必须
//! 二选一定一个真实 URL，并校验 driver 与 URL scheme 一致：
//!
//! | driver   | DATABASE_URL  | 结果                                             |
//! |----------|---------------|--------------------------------------------------|
//! | sqlite   | (sqlite URL)  | 用 env URL                                       |
//! | sqlite   | (postgres URL)| `UnsupportedBackend("driver/URL scheme mismatch")` |
//! | sqlite   | None          | fallback `sqlite://<sqlite_path>`                |
//! | postgres | (postgres URL)| 用 env URL                                       |
//! | postgres | (sqlite URL)  | `UnsupportedBackend("driver/URL scheme mismatch")` |
//! | postgres | None          | `UnsupportedBackend("driver=postgres requires DATABASE_URL")` |
//!
//! 失败统一走 `CliError::Storage(UnsupportedBackend)`，复用现有 exit
//! 行为（CliError::Storage → RuntimeError exit）。Diagnostic 通路可在
//! P3-A-fix2 / config-versioning 阶段再升级；本期只需启动期 fail-fast。

use std::path::Path;

use rss_ai_news_config::{DatabaseDriver, LoadedConfig};
use rss_ai_news_storage::{StorageError, StoragePool};

use crate::error::CliError;

/// 返回最终用于 [`StoragePool::build`] 的 URL，失败给出明确诊断。
pub fn resolve_storage_url(loaded: &LoadedConfig) -> Result<String, CliError> {
    resolve_storage_url_parts(
        loaded.app.database.driver,
        loaded.env.database_url.as_deref(),
        &loaded.app.database.sqlite_path,
    )
}

/// 字段级版本，便于单测不构造完整 [`LoadedConfig`]。空白 / 空字符串 URL
/// 视同未提供。
pub fn resolve_storage_url_parts(
    driver: DatabaseDriver,
    database_url: Option<&str>,
    sqlite_path: &Path,
) -> Result<String, CliError> {
    let url_env = database_url.map(str::trim).filter(|s| !s.is_empty());

    match (driver, url_env) {
        (DatabaseDriver::Sqlite, Some(url)) => {
            if StoragePool::is_postgres_url(url) {
                return Err(mismatch(
                    "driver=sqlite but DATABASE_URL has postgres scheme",
                ));
            }
            Ok(url.to_string())
        }
        (DatabaseDriver::Sqlite, None) => {
            // 注意：Windows 路径 `C:\path\file.db` 带反斜杠；`sqlite://`
            // scheme 后面 sqlx 通过 strip + Path::new 兼容平台分隔符（见
            // build_sqlite_pool）。这里只负责拼字符串，不做平台规范化。
            Ok(format!("sqlite://{}", sqlite_path.display()))
        }
        (DatabaseDriver::Postgres, Some(url)) => {
            if !StoragePool::is_postgres_url(url) {
                return Err(mismatch(
                    "driver=postgres but DATABASE_URL is not postgres scheme",
                ));
            }
            Ok(url.to_string())
        }
        (DatabaseDriver::Postgres, None) => {
            Err(CliError::Storage(StorageError::UnsupportedBackend(
                "driver=postgres requires DATABASE_URL (env or .env file)".into(),
            )))
        }
    }
}

fn mismatch(msg: &str) -> CliError {
    CliError::Storage(StorageError::UnsupportedBackend(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sqlite_path() -> PathBuf {
        PathBuf::from("data/test.db")
    }

    #[test]
    fn sqlite_with_sqlite_url_uses_env() {
        let url = resolve_storage_url_parts(
            DatabaseDriver::Sqlite,
            Some("sqlite://other.db"),
            &sqlite_path(),
        )
        .unwrap();
        assert_eq!(url, "sqlite://other.db");
    }

    #[test]
    fn sqlite_with_postgres_url_fails() {
        let err = resolve_storage_url_parts(
            DatabaseDriver::Sqlite,
            Some("postgres://u@h/db"),
            &sqlite_path(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CliError::Storage(StorageError::UnsupportedBackend(_))
        ));
    }

    #[test]
    fn sqlite_without_url_falls_back_to_path() {
        let url = resolve_storage_url_parts(DatabaseDriver::Sqlite, None, &sqlite_path()).unwrap();
        assert!(url.starts_with("sqlite://"));
        assert!(url.contains("test.db"));
    }

    #[test]
    fn postgres_with_postgres_url_uses_env() {
        let url = resolve_storage_url_parts(
            DatabaseDriver::Postgres,
            Some("postgres://u@h/db"),
            &sqlite_path(),
        )
        .unwrap();
        assert_eq!(url, "postgres://u@h/db");
    }

    #[test]
    fn postgres_with_sqlite_url_fails() {
        let err =
            resolve_storage_url_parts(DatabaseDriver::Postgres, Some("sqlite://x"), &sqlite_path())
                .unwrap_err();
        assert!(matches!(
            err,
            CliError::Storage(StorageError::UnsupportedBackend(_))
        ));
    }

    #[test]
    fn postgres_without_url_fails() {
        let err =
            resolve_storage_url_parts(DatabaseDriver::Postgres, None, &sqlite_path()).unwrap_err();
        match err {
            CliError::Storage(StorageError::UnsupportedBackend(msg)) => {
                assert!(msg.contains("DATABASE_URL"));
            }
            other => panic!("expected UnsupportedBackend, got {other:?}"),
        }
    }

    #[test]
    fn empty_url_is_treated_as_none() {
        let url =
            resolve_storage_url_parts(DatabaseDriver::Sqlite, Some("   "), &sqlite_path()).unwrap();
        assert!(url.starts_with("sqlite://"));
        assert!(url.contains("test.db"));
    }
}
