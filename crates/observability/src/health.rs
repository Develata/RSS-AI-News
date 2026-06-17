use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use reqwest::Client;
use rss_ai_news_config::LoadedConfig;
use rss_ai_news_storage::StoragePool;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;

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
        pool: StoragePool,
    }

    impl DatabaseConnectivityCheck {
        pub fn new(pool: StoragePool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl HealthCheck for DatabaseConnectivityCheck {
        fn name(&self) -> &str {
            "Database connection"
        }

        async fn run(&self) -> CheckOutcome {
            // codex P4 评审 HIGH-1 修复：PG 上 `SELECT 1` 的 `1` 字面量推断为
            // INT4，decode `i64` 会因类型 mismatch 失败让 doctor 误报 DB 不可达。
            // 改 `i32`：SQLite INTEGER storage class 兼容 `i32` decode（与
            // storage-multi-dialect §5.2 第 4 行 EXISTS CASE WHEN decode `i32` 同模式）。
            let result = match &self.pool {
                StoragePool::Sqlite(p) => {
                    sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(p).await
                }
                StoragePool::Postgres(p) => {
                    sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(p).await
                }
            };
            let backend = match &self.pool {
                StoragePool::Sqlite(_) => "SQLite",
                StoragePool::Postgres(_) => "PostgreSQL",
            };
            match result {
                Ok(1) => CheckOutcome::Ok(format!("{backend} reachable")),
                Ok(value) => CheckOutcome::Fail(format!("unexpected SELECT 1 result: {value}")),
                Err(error) => CheckOutcome::Fail(format!("{backend} query failed: {error}")),
            }
        }
    }
}

pub mod migration_check {
    use super::*;

    pub struct MigrationVersionCheck {
        pool: StoragePool,
        expected_version: i64,
    }

    impl MigrationVersionCheck {
        pub fn new(pool: StoragePool) -> Self {
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
            // W11-P4-C2：_sqlx_migrations 是 sqlx 框架表，SQLite/PG 同名同字段。
            // PG `MAX(version)` decode `Option<i64>` 也工作（_sqlx_migrations.version BIGINT）。
            let result = match &self.pool {
                StoragePool::Sqlite(p) => {
                    sqlx::query_scalar::<_, Option<i64>>(
                        "SELECT MAX(version) FROM _sqlx_migrations",
                    )
                    .fetch_one(p)
                    .await
                }
                StoragePool::Postgres(p) => {
                    sqlx::query_scalar::<_, Option<i64>>(
                        "SELECT MAX(version) FROM _sqlx_migrations",
                    )
                    .fetch_one(p)
                    .await
                }
            };
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
        pool: StoragePool,
    }

    impl ExpiredLeaseCheck {
        pub fn new(pool: StoragePool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl HealthCheck for ExpiredLeaseCheck {
        fn name(&self) -> &str {
            "Expired leases"
        }

        async fn run(&self) -> CheckOutcome {
            // W11-P4-C2：`?` → `$1` 跨方言占位符（SQLite 也支持 $N）。
            let sql = "SELECT COUNT(*) FROM article_ai_results WHERE state = 'running' AND lease_expires_at < $1";
            let now = OffsetDateTime::now_utc();
            let result = match &self.pool {
                StoragePool::Sqlite(p) => {
                    sqlx::query_scalar::<_, i64>(sql)
                        .bind(now)
                        .fetch_one(p)
                        .await
                }
                StoragePool::Postgres(p) => {
                    sqlx::query_scalar::<_, i64>(sql)
                        .bind(now)
                        .fetch_one(p)
                        .await
                }
            };
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
        pool: StoragePool,
    }

    impl FailedBacklogCheck {
        pub fn new(pool: StoragePool) -> Self {
            Self { pool }
        }
    }

    #[async_trait]
    impl HealthCheck for FailedBacklogCheck {
        fn name(&self) -> &str {
            "Failed backlog"
        }

        async fn run(&self) -> CheckOutcome {
            // W11-P4-C2：3 个 COUNT(*) 标量子查询跨方言等价；PG/SQLite
            // 都返 BIGINT/INT64，decode i64 OK；row.get::<i64, _> 跨方言通用。
            // codex P4 评审 MEDIUM-3 修复：publish_records 失败状态枚举值是
            // 'failed'（见 PublishState::Failed → "failed"），不是
            // 'permanent_failed'（后者仅 article_ai_results 用）。原写法在
            // 任何 publish 失败下都漏报 0，让运维永远看不到 publish 积压。
            let sql = r#"
                SELECT
                    (SELECT COUNT(*) FROM feed_entries WHERE state = 'failed') AS failed_entries,
                    (SELECT COUNT(*) FROM article_ai_results WHERE state = 'permanent_failed') AS failed_ai,
                    (SELECT COUNT(*) FROM publish_records WHERE state = 'failed') AS failed_publish
                "#;
            let result: Result<(i64, i64, i64), sqlx::Error> = match &self.pool {
                StoragePool::Sqlite(p) => sqlx::query(sql)
                    .fetch_one(p)
                    .await
                    .map(|row| (row.get(0), row.get(1), row.get(2))),
                StoragePool::Postgres(p) => sqlx::query(sql)
                    .fetch_one(p)
                    .await
                    .map(|row| (row.get(0), row.get(1), row.get(2))),
            };
            match result {
                Ok((e, a, p)) => {
                    let count = e + a + p;
                    CheckOutcome::Info(format!("{count} permanently failed entries"))
                }
                Err(error) => CheckOutcome::Fail(format!("failed backlog query failed: {error}")),
            }
        }
    }
}

pub mod stuck_reindex_check {
    use super::*;

    /// reindex job 卡住检测（3am 场景①）。两种卡死信号：
    /// - `running` + 租约过期：worker 中途死亡。该 job 因 partial-unique index
    ///   `(target) WHERE state IN ('pending','running')` 会**静默挡住**该 target
    ///   的后续 reindex，故必须可见。
    /// - `pending` 滞留超阈值：建 job 后没有任何 reindex run 来 claim（多半 run
    ///   在建 job 后崩溃）。
    ///
    /// 两者都 Warn——可由再次运行 reindex（启动期 reclaim + 重新 claim）自愈，
    /// 但需运维知晓。
    pub struct StuckReindexCheck {
        pool: StoragePool,
        pending_threshold_secs: u64,
    }

    impl StuckReindexCheck {
        pub fn new(pool: StoragePool, pending_threshold_secs: u64) -> Self {
            Self {
                pool,
                pending_threshold_secs,
            }
        }
    }

    #[async_trait]
    impl HealthCheck for StuckReindexCheck {
        fn name(&self) -> &str {
            "Stuck reindex jobs"
        }

        async fn run(&self) -> CheckOutcome {
            let now = OffsetDateTime::now_utc();
            let pending_cutoff = now
                - time::Duration::seconds(
                    i64::try_from(self.pending_threshold_secs).unwrap_or(i64::MAX),
                );
            // $1=now（running 租约过期判定），$2=pending_cutoff（pending 滞留判定）。
            // created_at 由 INSERT_REINDEX_JOB_PENDING_SQL 绑定 OffsetDateTime
            // 写入（RFC3339），与 $2 同格式可比。
            let sql = r#"
                SELECT
                    (SELECT COUNT(*) FROM reindex_jobs
                     WHERE state = 'running'
                       AND lease_expires_at IS NOT NULL
                       AND lease_expires_at < $1) AS expired_running,
                    (SELECT COUNT(*) FROM reindex_jobs
                     WHERE state = 'pending'
                       AND created_at < $2) AS stale_pending
            "#;
            let result: Result<(i64, i64), sqlx::Error> = match &self.pool {
                StoragePool::Sqlite(p) => sqlx::query(sql)
                    .bind(now)
                    .bind(pending_cutoff)
                    .fetch_one(p)
                    .await
                    .map(|row| (row.get(0), row.get(1))),
                StoragePool::Postgres(p) => sqlx::query(sql)
                    .bind(now)
                    .bind(pending_cutoff)
                    .fetch_one(p)
                    .await
                    .map(|row| (row.get(0), row.get(1))),
            };
            match result {
                Ok((0, 0)) => CheckOutcome::Ok("0 stuck reindex jobs".to_string()),
                Ok((expired, stale)) => CheckOutcome::Warn(format!(
                    "{expired} reindex jobs with expired running lease, {stale} pending past threshold"
                )),
                Err(error) => CheckOutcome::Fail(format!("stuck reindex query failed: {error}")),
            }
        }
    }
}

pub mod silent_source_check {
    use super::*;

    /// 静默 source 检测（3am 场景②）。两种静默信号，仅看 `active` source：
    /// - `COALESCE(last_success_at, created_at)` 早于阈值：曾成功后变哑，**或**
    ///   从未成功（回落到创建时间）——后者覆盖"建后调度/worker 停摆、从未成功
    ///   且失败计数为 0"的源（codex P2：此前 `last_success_at IS NOT NULL` 谓词
    ///   会漏掉这个核心场景）。
    /// - `consecutive_failures` 达阈值：持续失败。
    ///
    /// 数据由 ingest / config seed 维护。`created_at` 由 feed_sources upsert 绑定
    /// `OffsetDateTime` 写入（RFC3339 / TIMESTAMPTZ），与 `last_success_at` 同
    /// 格式、与 `$1` 可比——故 `COALESCE` 在两方言下都是正确的时间比较。
    pub struct SilentSourceCheck {
        pool: StoragePool,
        max_age_secs: u64,
        max_consecutive_failures: u32,
    }

    impl SilentSourceCheck {
        pub fn new(pool: StoragePool, max_age_secs: u64, max_consecutive_failures: u32) -> Self {
            Self {
                pool,
                max_age_secs,
                max_consecutive_failures,
            }
        }
    }

    #[async_trait]
    impl HealthCheck for SilentSourceCheck {
        fn name(&self) -> &str {
            "Silent feed sources"
        }

        async fn run(&self) -> CheckOutcome {
            let stale_cutoff = OffsetDateTime::now_utc()
                - time::Duration::seconds(i64::try_from(self.max_age_secs).unwrap_or(i64::MAX));
            let max_fail = i64::from(self.max_consecutive_failures);
            // $1=stale_cutoff（与 last_success_at / created_at 同 RFC3339/TIMESTAMPTZ
            // 格式可比），$2=max_fail。COALESCE 让从未成功的源回落到 created_at 计龄。
            let sql = r#"
                SELECT
                    (SELECT COUNT(*) FROM feed_sources
                     WHERE status = 'active'
                       AND COALESCE(last_success_at, created_at) < $1) AS stale,
                    (SELECT COUNT(*) FROM feed_sources
                     WHERE status = 'active'
                       AND consecutive_failures >= $2) AS failing
            "#;
            let result: Result<(i64, i64), sqlx::Error> = match &self.pool {
                StoragePool::Sqlite(p) => sqlx::query(sql)
                    .bind(stale_cutoff)
                    .bind(max_fail)
                    .fetch_one(p)
                    .await
                    .map(|row| (row.get(0), row.get(1))),
                StoragePool::Postgres(p) => sqlx::query(sql)
                    .bind(stale_cutoff)
                    .bind(max_fail)
                    .fetch_one(p)
                    .await
                    .map(|row| (row.get(0), row.get(1))),
            };
            match result {
                Ok((0, 0)) => CheckOutcome::Ok("0 silent sources".to_string()),
                Ok((stale, failing)) => CheckOutcome::Warn(format!(
                    "{stale} active sources stale (no recent success), {failing} over consecutive-failure threshold"
                )),
                Err(error) => CheckOutcome::Fail(format!("silent source query failed: {error}")),
            }
        }
    }
}

pub mod pending_backlog_check {
    use super::*;

    /// 待处理积压检测（3am 场景③）。数**健康但堆积**的待处理队列深度：
    /// `pending_fetch`（抓取队列）与 `pending` AI 任务队列。区别于
    /// `FailedBacklogCheck`（数终态失败）与 deep-scan I9（数预算耗尽的滞留行）——
    /// 本检查抓的是 worker 跟不上 / 调度未运行 / 流量尖峰导致的队列增长。
    pub struct PendingBacklogCheck {
        pool: StoragePool,
        warn_threshold: u64,
    }

    impl PendingBacklogCheck {
        pub fn new(pool: StoragePool, warn_threshold: u64) -> Self {
            Self {
                pool,
                warn_threshold,
            }
        }
    }

    #[async_trait]
    impl HealthCheck for PendingBacklogCheck {
        fn name(&self) -> &str {
            "Pending backlog"
        }

        async fn run(&self) -> CheckOutcome {
            let sql = r#"
                SELECT
                    (SELECT COUNT(*) FROM feed_entries WHERE state = 'pending_fetch') AS fetch_queue,
                    (SELECT COUNT(*) FROM article_ai_results WHERE state = 'pending') AS ai_queue
            "#;
            let result: Result<(i64, i64), sqlx::Error> = match &self.pool {
                StoragePool::Sqlite(p) => sqlx::query(sql)
                    .fetch_one(p)
                    .await
                    .map(|row| (row.get(0), row.get(1))),
                StoragePool::Postgres(p) => sqlx::query(sql)
                    .fetch_one(p)
                    .await
                    .map(|row| (row.get(0), row.get(1))),
            };
            match result {
                Ok((fetch_q, ai_q)) => {
                    let threshold = i64::try_from(self.warn_threshold).unwrap_or(i64::MAX);
                    if fetch_q >= threshold || ai_q >= threshold {
                        CheckOutcome::Warn(format!(
                            "fetch queue {fetch_q}, AI queue {ai_q} (warn threshold {threshold})"
                        ))
                    } else {
                        CheckOutcome::Ok(format!("fetch queue {fetch_q}, AI queue {ai_q}"))
                    }
                }
                Err(error) => CheckOutcome::Fail(format!("pending backlog query failed: {error}")),
            }
        }
    }
}
