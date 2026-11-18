//! Resolve the configured database location before opening storage.
//!
//! `DATABASE_URL` must use a supported scheme and agree with `database.driver`.
//! SQLite also accepts a bare path; without a URL it uses `database.sqlite_path`.
//! PostgreSQL requires an explicit URL. Invalid combinations produce a config
//! diagnostic (exit 78) that never includes credentials or the original URL.

use std::path::Path;

use rss_ai_news_config::{ConfigError, DatabaseDriver, Diagnostic, DiagnosticReport, LoadedConfig};
use rss_ai_news_storage::{
    PG_HOSTADDR_UNSUPPORTED, StoragePool, pg_hostaddr_in_use, pghostaddr_set,
};

use crate::error::CliError;

/// 返回最终用于 [`StoragePool::build`] 的 URL，失败给出明确诊断。
pub fn resolve_storage_url(loaded: &LoadedConfig) -> Result<String, CliError> {
    let url = resolve_storage_url_parts(
        loaded.app.database.driver,
        loaded.env.database_url.as_deref(),
        &loaded.app.database.sqlite_path,
    )?;
    check_pg_hostaddr(
        loaded.app.database.driver,
        loaded.env.database_url.as_deref(),
        pghostaddr_set(),
    )?;
    Ok(url)
}

/// PostgreSQL 不支持 `hostaddr`（见 [`pg_hostaddr_in_use`]）：在配置阶段拒绝，
/// exit 78，而不是等建池时以运行时错误失败。数据库命令（经
/// [`resolve_storage_url`]）与 `validate-config` 共用。SQLite 不读这些设置，不检查。
///
/// `pghostaddr_set` 只反映进程环境：`.env` 文件里的值不会导出到进程环境，SQLx 也
/// 看不到它。
pub fn check_pg_hostaddr(
    driver: DatabaseDriver,
    database_url: Option<&str>,
    pghostaddr_set: bool,
) -> Result<(), CliError> {
    let url = database_url.map(str::trim).unwrap_or_default();
    if !matches!(driver, DatabaseDriver::Postgres) || !pg_hostaddr_in_use(url, pghostaddr_set) {
        return Ok(());
    }
    let field_path = if pghostaddr_set {
        "env.pghostaddr"
    } else {
        "env.database_url"
    };
    Err(CliError::Config(ConfigError::ValidationFailed {
        report: DiagnosticReport::new(vec![Diagnostic::new(
            "env",
            field_path,
            PG_HOSTADDR_UNSUPPORTED,
        )]),
    }))
}

/// 字段级版本，便于单测不构造完整 [`LoadedConfig`]。空白 / 空字符串 URL
/// 视同未提供。
pub fn resolve_storage_url_parts(
    driver: DatabaseDriver,
    database_url: Option<&str>,
    sqlite_path: &Path,
) -> Result<String, CliError> {
    let url_env = database_url.map(str::trim).filter(|s| !s.is_empty());

    if let Some(url) = url_env {
        StoragePool::validate_url_scheme(url).map_err(|_| {
            driver_url_mismatch(
                "env.database_url",
                "DATABASE_URL must use sqlite, postgres, or postgresql scheme, or a bare SQLite path",
            )
        })?;
    }

    match (driver, url_env) {
        (DatabaseDriver::Sqlite, Some(url)) => {
            if StoragePool::is_postgres_url(url) {
                return Err(driver_url_mismatch(
                    "database.driver",
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
                return Err(driver_url_mismatch(
                    "database.driver",
                    "driver=postgres but DATABASE_URL is not postgres scheme",
                ));
            }
            Ok(url.to_string())
        }
        (DatabaseDriver::Postgres, None) => Err(driver_url_mismatch(
            "env.database_url",
            "driver=postgres requires DATABASE_URL (env or .env file)",
        )),
    }
}

/// codex P4 评审 MEDIUM-1 修复：driver/URL 错配应走 `ConfigError::ValidationFailed`
/// （exit 78），而不是 `StorageError::UnsupportedBackend` 包装成 RuntimeError
/// （exit 1）。设计 storage-multi-dialect §5.4 显式规定走 `DiagnosticReport`
/// 通路，与 README 退出码表对齐。
fn driver_url_mismatch(field_path: &str, message: &str) -> CliError {
    CliError::Config(ConfigError::ValidationFailed {
        report: DiagnosticReport::new(vec![Diagnostic::new("env", field_path, message)]),
    })
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
        // codex P4 fix1.M1：driver/URL 错配走 ConfigError exit 78
        assert!(matches!(
            err,
            CliError::Config(ConfigError::ValidationFailed { .. })
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
            CliError::Config(ConfigError::ValidationFailed { .. })
        ));
    }

    #[test]
    fn unsupported_url_scheme_is_config_error_without_secrets() {
        for scheme in ["mysql", "http", "foo"] {
            let url = format!("{scheme}://private_user:private_password@host/db?token=secret");
            for driver in [DatabaseDriver::Sqlite, DatabaseDriver::Postgres] {
                let error = resolve_storage_url_parts(driver, Some(&url), &sqlite_path())
                    .expect_err("unsupported URL scheme must fail during configuration");
                assert!(matches!(
                    error,
                    CliError::Config(ConfigError::ValidationFailed { .. })
                ));
                let diagnostic = format!("{error:?}");
                for secret in ["private_user", "private_password", "token=secret"] {
                    assert!(!diagnostic.contains(secret));
                }
            }
        }
    }

    #[test]
    fn postgres_without_url_fails() {
        let err =
            resolve_storage_url_parts(DatabaseDriver::Postgres, None, &sqlite_path()).unwrap_err();
        match err {
            CliError::Config(ConfigError::ValidationFailed { report }) => {
                let msg = report.diagnostics[0].message.clone();
                assert!(msg.contains("DATABASE_URL"));
                assert_eq!(report.diagnostics[0].field_path, "env.database_url");
            }
            other => panic!("expected Config::ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn pg_hostaddr_is_a_config_error_without_secrets() {
        let plain = "postgres://private_user:private_password@db.example/news";
        let with_param = format!("{plain}?hostaddr=10.0.0.9");
        for (url, pghostaddr_set, field) in [
            (Some(plain), true, "env.pghostaddr"),
            (None, true, "env.pghostaddr"),
            (Some(with_param.as_str()), false, "env.database_url"),
            (Some(with_param.as_str()), true, "env.pghostaddr"),
        ] {
            let error =
                check_pg_hostaddr(DatabaseDriver::Postgres, url, pghostaddr_set).unwrap_err();
            assert!(matches!(
                error.exit_code(),
                crate::exit_code::ExitCode::ConfigError
            ));
            let CliError::Config(ConfigError::ValidationFailed { report }) = &error else {
                panic!("expected Config::ValidationFailed, got {error:?}");
            };
            assert_eq!(report.diagnostics[0].field_path, field);
            let diagnostic = format!("{error:?} {}", error.display_user());
            for secret in ["private_user", "private_password"] {
                assert!(!diagnostic.contains(secret), "{diagnostic}");
            }
        }
    }

    #[test]
    fn pg_without_hostaddr_and_sqlite_pass_the_hostaddr_check() {
        let pg = DatabaseDriver::Postgres;
        check_pg_hostaddr(pg, Some("postgres://u:p@db.example/news"), false).unwrap();
        check_pg_hostaddr(pg, Some("postgres://u:p@db.example/news?host=other"), false).unwrap();
        check_pg_hostaddr(pg, None, false).unwrap();
        let sqlite = DatabaseDriver::Sqlite;
        check_pg_hostaddr(sqlite, Some("sqlite://data/news.db"), true).unwrap();
        check_pg_hostaddr(sqlite, Some("data/news.db?hostaddr=10.0.0.9"), true).unwrap();
        check_pg_hostaddr(sqlite, None, true).unwrap();
    }

    #[test]
    fn empty_url_is_treated_as_none() {
        let url =
            resolve_storage_url_parts(DatabaseDriver::Sqlite, Some("   "), &sqlite_path()).unwrap();
        assert!(url.starts_with("sqlite://"));
        assert!(url.contains("test.db"));
    }
}
