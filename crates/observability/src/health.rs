use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use reqwest::Client;
use rss_ai_news_config::LoadedConfig;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::redact::{redact_authorization_header, redact_url_userinfo};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "message", rename_all = "snake_case")]
pub enum CheckOutcome {
    Ok(String),
    Warn(String),
    Fail(String),
    Info(String),
}

impl CheckOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Ok(_) => "ok",
            Self::Warn(_) => "warn",
            Self::Fail(_) => "fail",
            Self::Info(_) => "info",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Ok(message) | Self::Warn(message) | Self::Fail(message) | Self::Info(message) => {
                message
            }
        }
    }
}

#[async_trait]
pub trait HealthCheck: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self) -> CheckOutcome;
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CheckReport {
    pub items: Vec<(String, CheckOutcome)>,
}

impl CheckReport {
    pub fn has_fail(&self) -> bool {
        self.items
            .iter()
            .any(|(_, outcome)| matches!(outcome, CheckOutcome::Fail(_)))
    }

    pub fn has_warn(&self) -> bool {
        self.items
            .iter()
            .any(|(_, outcome)| matches!(outcome, CheckOutcome::Warn(_)))
    }
}

pub mod config_check {
    use super::*;

    pub struct ConfigCheck {
        loaded: Arc<LoadedConfig>,
    }

    impl ConfigCheck {
        pub fn new(loaded: Arc<LoadedConfig>) -> Self {
            Self { loaded }
        }
    }

    #[async_trait]
    impl HealthCheck for ConfigCheck {
        fn name(&self) -> &str {
            "Configuration"
        }

        async fn run(&self) -> CheckOutcome {
            let categories = self.loaded.categories.len();
            CheckOutcome::Ok(format!("valid ({categories} categories)"))
        }
    }
}

pub mod db_check {
    use super::*;

    pub struct DatabaseConnectivityCheck {
        pool: SqlitePool,
    }

    impl DatabaseConnectivityCheck {
        pub fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl HealthCheck for DatabaseConnectivityCheck {
        fn name(&self) -> &str {
            "Database connection"
        }

        async fn run(&self) -> CheckOutcome {
            match sqlx::query_scalar::<_, i64>("SELECT 1")
                .fetch_one(&self.pool)
                .await
            {
                Ok(1) => CheckOutcome::Ok("SQLite reachable".to_string()),
                Ok(value) => CheckOutcome::Fail(format!("unexpected SELECT 1 result: {value}")),
                Err(error) => CheckOutcome::Fail(format!("SQLite query failed: {error}")),
            }
        }
    }
}

pub mod migration_check {
    use super::*;

    pub struct MigrationVersionCheck {
        pool: SqlitePool,
        expected_version: i64,
    }

    impl MigrationVersionCheck {
        pub fn new(pool: SqlitePool) -> Self {
            Self {
                pool,
                expected_version: 1,
            }
        }
    }

    #[async_trait]
    impl HealthCheck for MigrationVersionCheck {
        fn name(&self) -> &str {
            "Migration version"
        }

        async fn run(&self) -> CheckOutcome {
            let result =
                sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(version) FROM _sqlx_migrations")
                    .fetch_one(&self.pool)
                    .await;
            match result {
                Ok(Some(version)) if version >= self.expected_version => {
                    CheckOutcome::Ok(format!("{version:04} (up to date)"))
                }
                Ok(Some(version)) => CheckOutcome::Fail(format!(
                    "{version:04} (expected at least {:04})",
                    self.expected_version
                )),
                Ok(None) => CheckOutcome::Fail("no migrations applied".to_string()),
                Err(error) => CheckOutcome::Fail(format!("migration table unavailable: {error}")),
            }
        }
    }
}

pub mod openai_check {
    use super::*;

    pub struct OpenAiPingCheck {
        http: Client,
        base_url: Option<String>,
        api_key: Option<String>,
        model: String,
        enabled: bool,
    }

    impl OpenAiPingCheck {
        pub fn new(
            http: Client,
            base_url: Option<String>,
            api_key: Option<String>,
            model: String,
            enabled: bool,
        ) -> Self {
            Self {
                http,
                base_url,
                api_key,
                model,
                enabled,
            }
        }
    }

    #[async_trait]
    impl HealthCheck for OpenAiPingCheck {
        fn name(&self) -> &str {
            "OpenAI API key"
        }

        async fn run(&self) -> CheckOutcome {
            if !self.enabled {
                return CheckOutcome::Info("skipped (app.ai.enabled=false)".to_string());
            }
            let Some(api_key) = self
                .api_key
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            else {
                return CheckOutcome::Info("skipped (OPENAI_API_KEY not configured)".to_string());
            };

            let base = self
                .base_url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/');
            let url = format!("{base}/chat/completions");
            let response = self
                .http
                .post(&url)
                .bearer_auth(api_key)
                .json(&serde_json::json!({
                    "model": self.model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1,
                    "stream": false,
                }))
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    let msg = redact_authorization_header(&error.to_string()).into_owned();
                    return CheckOutcome::Fail(format!("request failed: {msg}"));
                }
            };
            let status = response.status();
            if !status.is_success() {
                return CheckOutcome::Fail(format!("HTTP {status}"));
            }
            match response.json::<serde_json::Value>().await {
                Ok(value)
                    if value
                        .get("choices")
                        .and_then(|choices| choices.as_array())
                        .is_some() =>
                {
                    CheckOutcome::Ok(format!("valid (model: {})", self.model))
                }
                Ok(_) => CheckOutcome::Fail("invalid chat completion JSON".to_string()),
                Err(error) => CheckOutcome::Fail(format!("invalid JSON: {error}")),
            }
        }
    }
}

pub mod github_check {
    use super::*;

    pub struct GitHubPingCheck {
        http: Client,
        token: Option<String>,
        api_base: String,
    }

    impl GitHubPingCheck {
        pub fn new(http: Client, token: Option<String>) -> Self {
            Self::with_base_url(http, token, "https://api.github.com")
        }

        pub fn with_base_url(
            http: Client,
            token: Option<String>,
            api_base: impl Into<String>,
        ) -> Self {
            Self {
                http,
                token,
                api_base: api_base.into(),
            }
        }
    }

    #[async_trait]
    impl HealthCheck for GitHubPingCheck {
        fn name(&self) -> &str {
            "GitHub token"
        }

        async fn run(&self) -> CheckOutcome {
            let Some(token) = self
                .token
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            else {
                return CheckOutcome::Warn("not configured (publish will fail)".to_string());
            };
            let url = format!("{}/user", self.api_base.trim_end_matches('/'));
            let response = self
                .http
                .get(url)
                .bearer_auth(token)
                .header("User-Agent", "rss-ai-news-doctor")
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    CheckOutcome::Ok("valid".to_string())
                }
                Ok(response) => CheckOutcome::Fail(format!("HTTP {}", response.status())),
                Err(error) => {
                    let msg = redact_authorization_header(&error.to_string()).into_owned();
                    CheckOutcome::Fail(format!("request failed: {msg}"))
                }
            }
        }
    }
}

pub mod rsshub_check {
    use super::*;

    pub struct RsshubPingCheck {
        http: Client,
        base_url: Option<String>,
    }

    impl RsshubPingCheck {
        pub fn new(http: Client, base_url: Option<String>) -> Self {
            Self { http, base_url }
        }
    }

    #[async_trait]
    impl HealthCheck for RsshubPingCheck {
        fn name(&self) -> &str {
            "RSSHub base URL"
        }

        async fn run(&self) -> CheckOutcome {
            let Some(base_url) = self
                .base_url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            else {
                return CheckOutcome::Info("not configured".to_string());
            };
            let safe_url = redact_url_userinfo(base_url).into_owned();
            match self.http.get(base_url).send().await {
                Ok(response) if response.status().is_success() => {
                    CheckOutcome::Ok(format!("{safe_url} (reachable)"))
                }
                Ok(response) => {
                    CheckOutcome::Fail(format!("{safe_url} returned {}", response.status()))
                }
                Err(error) => CheckOutcome::Fail(format!("{safe_url} unreachable: {error}")),
            }
        }
    }
}

pub mod timezone_check {
    use super::*;

    pub struct TimezoneCheck {
        timezone: String,
    }

    impl TimezoneCheck {
        pub fn new(timezone: String) -> Self {
            Self { timezone }
        }
    }

    #[async_trait]
    impl HealthCheck for TimezoneCheck {
        fn name(&self) -> &str {
            "Timezone"
        }

        async fn run(&self) -> CheckOutcome {
            let trimmed = self.timezone.trim();
            if trimmed.is_empty() {
                CheckOutcome::Fail("empty timezone".to_string())
            } else if trimmed.contains('/') || trimmed.eq_ignore_ascii_case("UTC") {
                CheckOutcome::Ok(trimmed.to_string())
            } else {
                CheckOutcome::Warn(format!("{trimmed} (basic validation only)"))
            }
        }
    }
}

pub mod disk_check {
    use super::*;

    pub struct DiskSpaceCheck {
        path: PathBuf,
        min_free_bytes: u64,
    }

    impl DiskSpaceCheck {
        pub fn new(path: PathBuf, min_free_bytes: u64) -> Self {
            Self {
                path,
                min_free_bytes,
            }
        }
    }

    #[async_trait]
    impl HealthCheck for DiskSpaceCheck {
        fn name(&self) -> &str {
            "Disk space"
        }

        async fn run(&self) -> CheckOutcome {
            let path = if self.path.is_dir() {
                self.path.clone()
            } else {
                self.path
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
            };
            match fs2::available_space(&path) {
                Ok(bytes) if bytes > self.min_free_bytes => {
                    CheckOutcome::Ok(format!("{:.1} GB free", bytes as f64 / 1_073_741_824.0))
                }
                Ok(bytes) => CheckOutcome::Fail(format!(
                    "{:.1} MB free (minimum {:.1} MB)",
                    bytes as f64 / 1_048_576.0,
                    self.min_free_bytes as f64 / 1_048_576.0
                )),
                Err(error) => CheckOutcome::Info(format!("disk-space check unavailable: {error}")),
            }
        }
    }
}

pub mod lease_check {
    use super::*;

    pub struct ExpiredLeaseCheck {
        pool: SqlitePool,
    }

    impl ExpiredLeaseCheck {
        pub fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl HealthCheck for ExpiredLeaseCheck {
        fn name(&self) -> &str {
            "Expired leases"
        }

        async fn run(&self) -> CheckOutcome {
            let result = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM article_ai_results WHERE state = 'running' AND lease_expires_at < datetime('now')",
            )
            .fetch_one(&self.pool)
            .await;
            match result {
                Ok(0) => CheckOutcome::Ok("0 expired leases".to_string()),
                Ok(count) => CheckOutcome::Warn(format!("{count} expired leases pending reclaim")),
                Err(error) => CheckOutcome::Fail(format!("expired lease query failed: {error}")),
            }
        }
    }
}

pub mod backlog_check {
    use super::*;

    pub struct FailedBacklogCheck {
        pool: SqlitePool,
    }

    impl FailedBacklogCheck {
        pub fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl HealthCheck for FailedBacklogCheck {
        fn name(&self) -> &str {
            "Failed backlog"
        }

        async fn run(&self) -> CheckOutcome {
            let row = sqlx::query(
                r#"
                SELECT
                    (SELECT COUNT(*) FROM feed_entries WHERE state = 'failed') AS failed_entries,
                    (SELECT COUNT(*) FROM article_ai_results WHERE state = 'permanent_failed') AS failed_ai,
                    (SELECT COUNT(*) FROM publish_records WHERE state = 'permanent_failed') AS failed_publish
                "#,
            )
            .fetch_one(&self.pool)
            .await;

            match row {
                Ok(row) => {
                    let count: i64 = row.get::<i64, _>("failed_entries")
                        + row.get::<i64, _>("failed_ai")
                        + row.get::<i64, _>("failed_publish");
                    CheckOutcome::Info(format!("{count} permanently failed entries"))
                }
                Err(error) => CheckOutcome::Fail(format!("failed backlog query failed: {error}")),
            }
        }
    }
}
