use std::io::{self, Write};

use rss_ai_news_config as config;
use rss_ai_news_storage::{
    ReindexJobRepository, SqliteReindexJobRepo, build_sqlite_pool, run_migrations,
};
use serde::Serialize;

use crate::{args::Cli, error::CliError, output::CommandSummary};

#[derive(Debug, Clone, Serialize)]
pub struct MigrateCommandSummary {
    pub action: String,
    pub applied_versions: Vec<i64>,
    pub current_version: Option<i64>,
}

impl CommandSummary for MigrateCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Migrate {} completed:", self.action)?;
        writeln!(
            writer,
            "  Current version: {}",
            self.current_version
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        )?;
        writeln!(
            writer,
            "  Applied migrations: {}",
            self.applied_versions.len()
        )
    }
}

// migrate is an infrastructure command: it only opens SQLite and runs the
// embedded schema migrations. It must not be gated by OPENAI_* / RSSHUB_BASE_URL
// env presence — those are business-credential concerns enforced for AI / fetch
// commands by `validate::run_general_checks`. See loader::load_skip_env_checks.
pub async fn run(cli: &Cli) -> Result<MigrateCommandSummary, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    let pool = open_pool(&loaded.app).await?;
    // F15-11 W9-F4：cli-semantics §4.8 line 312 —— `migrate run` 与
    // `running`/`pending` reindex_job 互斥。在执行任何 schema 升级前必须
    // 先确认无 active reindex（schema 与 rule-version 升级职责边界明确：
    // schema 走 migrate / rule version 走 reindex，二者不应交叉）。
    //
    // 注：本检查依赖 reindex_jobs 表已存在。若库尚未跑过 migration 0002
    // （即 reindex_jobs 表未创建），list_running 会因 missing table 返
    // StorageError；该路径仅在"全新 DB 首次 migrate run"时触发，此时
    // 也不可能有 active reindex_job，故捕获并视为 0 active job。
    assert_no_running_reindex(&pool).await?;
    run_migrations(&pool).await?;
    summary("run", &pool).await
}

pub async fn check(cli: &Cli) -> Result<MigrateCommandSummary, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    let pool = open_pool(&loaded.app).await?;
    // `migrate check` 仅查询版本号，**不阻塞**：cli-semantics §4.8 line 312
    // 只要求 `migrate run` 互斥；check 是只读探测，allowed during reindex
    // —— 否则 oncall 无法在排查时确认 schema 状态。
    summary("check", &pool).await
}

/// `migrate run` 阻塞门（F15-11）。若有 `pending`/`running` 状态的
/// reindex_jobs，返回 [`CliError::MigrateBlockedByRunningReindex`]。
///
/// **新库容错**：reindex_jobs 表由 migration 0002 创建；若调用方在 0002
/// 之前的 schema 上调用 list_running，sqlx 会返 "no such table" 错误。
/// 此时视为 0 active job 放行（新库 + 首次 migrate run 不存在 active
/// reindex_job 的物理可能性）；其它 StorageError 透传。
pub(crate) async fn assert_no_running_reindex(pool: &sqlx::SqlitePool) -> Result<(), CliError> {
    let repo = SqliteReindexJobRepo::new(pool.clone());
    let rows = match repo.list_running().await {
        Ok(rows) => rows,
        Err(rss_ai_news_storage::StorageError::Sqlx(error))
            if error.to_string().contains("no such table") =>
        {
            return Ok(());
        }
        Err(error) => return Err(CliError::Storage(error)),
    };
    if rows.is_empty() {
        return Ok(());
    }
    let job_ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    Err(CliError::MigrateBlockedByRunningReindex {
        count: job_ids.len(),
        job_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rss_ai_news_storage::{build_sqlite_pool, run_migrations};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

    async fn make_test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("temp dir");
        let counter = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let db_path = dir.path().join(format!("migrate-{counter}-{nanos}.sqlite"));
        let pool = build_sqlite_pool(&db_path, 1, 5_000).await.expect("pool");
        run_migrations(&pool).await.expect("migrations");
        (dir, pool)
    }

    /// 插入一个 rule_versions 行作为 reindex_jobs.rule_version_id FK target。
    /// 用 status='superseded' 绕开 partial unique（与 storage tests::common
    /// 同 idiom）。
    async fn insert_reindex_rule(pool: &sqlx::SqlitePool, tag: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, status) \
             VALUES ('reindex', ?, 'fixture', ?, 'superseded') RETURNING id",
        )
        .bind(tag)
        .bind(format!("sha-{tag}"))
        .fetch_one(pool)
        .await
        .expect("rule row")
    }

    /// 直接 INSERT 一行指定 state 的 reindex_jobs，绕过 storage 状态机，
    /// 用于构造各分支输入（partial unique 不允许两行同 target 都 active —
    /// 测试用不同 target key 隔离）。terminal 状态需带 finished_at（schema
    /// CHECK 约束 `state != 'completed' OR finished_at IS NOT NULL` 等）。
    async fn insert_reindex_job(
        pool: &sqlx::SqlitePool,
        target: &str,
        state: &str,
        rule_id: i64,
    ) -> i64 {
        let (finished_clause, error_clause, aborted_clause) = match state {
            "completed" => ("datetime('now')", "NULL", "NULL"),
            "failed" => ("datetime('now')", "'test error'", "NULL"),
            "aborted" => ("datetime('now')", "NULL", "'test reason'"),
            _ => ("NULL", "NULL", "NULL"),
        };
        let sql = format!(
            r#"
            INSERT INTO reindex_jobs (
                target, rule_version_id, state, attempt_count,
                lease_owner, lease_expires_at, finished_at, error, aborted_reason
            )
            VALUES (?, ?, ?, 0, NULL, NULL, {finished_clause}, {error_clause}, {aborted_clause})
            RETURNING id
            "#
        );
        sqlx::query_scalar::<_, i64>(&sql)
            .bind(target)
            .bind(rule_id)
            .bind(state)
            .fetch_one(pool)
            .await
            .expect("reindex_job row")
    }

    #[tokio::test]
    async fn empty_reindex_jobs_table_passes_gate() {
        let (_dir, pool) = make_test_pool().await;
        assert_no_running_reindex(&pool)
            .await
            .expect("no active job → Ok");
    }

    #[tokio::test]
    async fn pending_reindex_job_blocks_migrate_run() {
        let (_dir, pool) = make_test_pool().await;
        let rule_id = insert_reindex_rule(&pool, "v-pending").await;
        let job_id = insert_reindex_job(&pool, "link_hash", "pending", rule_id).await;

        let err = assert_no_running_reindex(&pool)
            .await
            .expect_err("pending must block");
        match err {
            CliError::MigrateBlockedByRunningReindex { count, job_ids } => {
                assert_eq!(count, 1);
                assert_eq!(job_ids, vec![job_id]);
            }
            other => panic!("expected MigrateBlockedByRunningReindex, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn running_reindex_job_blocks_migrate_run() {
        let (_dir, pool) = make_test_pool().await;
        let rule_id = insert_reindex_rule(&pool, "v-running").await;
        // running 需要 lease_owner 非空（schema 约束），用直接 SQL 写完整
        let job_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO reindex_jobs (
                target, rule_version_id, state, attempt_count,
                lease_owner, lease_expires_at, started_at
            )
            VALUES ('content_hash', ?, 'running', 1, 'worker-a', datetime('now', '+10 minute'), datetime('now'))
            RETURNING id
            "#,
        )
        .bind(rule_id)
        .fetch_one(&pool)
        .await
        .expect("running job");

        let err = assert_no_running_reindex(&pool)
            .await
            .expect_err("running must block");
        match err {
            CliError::MigrateBlockedByRunningReindex { count, job_ids } => {
                assert_eq!(count, 1);
                assert_eq!(job_ids, vec![job_id]);
            }
            other => panic!("expected MigrateBlockedByRunningReindex, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminal_only_reindex_jobs_do_not_block() {
        // 三种终态 completed/aborted/failed 都不应触发阻塞门。
        let (_dir, pool) = make_test_pool().await;
        let rule_id = insert_reindex_rule(&pool, "v-terminal").await;
        for (target, state) in [
            ("link_hash", "completed"),
            ("content_hash", "aborted"),
            ("categories", "failed"),
        ] {
            insert_reindex_job(&pool, target, state, rule_id).await;
        }
        assert_no_running_reindex(&pool)
            .await
            .expect("terminal-only → Ok");
    }

    #[tokio::test]
    async fn blocked_error_maps_to_runtime_exit_code() {
        // cli-semantics §6: 操作前提不满足 → RuntimeError → exit 1。
        let err = CliError::MigrateBlockedByRunningReindex {
            count: 2,
            job_ids: vec![17, 18],
        };
        assert_eq!(err.error_kind(), "migrate_blocked_by_running_reindex");
        assert_eq!(err.command_name(), "migrate");
        assert!(matches!(
            err.exit_code(),
            crate::exit_code::ExitCode::RuntimeError
        ));
        assert!(err.display_user().contains("17"));
        assert!(err.display_user().contains("18"));
    }

    #[tokio::test]
    async fn lists_all_blocking_job_ids_in_order() {
        // 多 target 同时挂着 pending+running 时，job_ids 必须包含全部。
        let (_dir, pool) = make_test_pool().await;
        let rule_id = insert_reindex_rule(&pool, "v-multi").await;
        let pending_id = insert_reindex_job(&pool, "link_hash", "pending", rule_id).await;
        let running_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO reindex_jobs (
                target, rule_version_id, state, attempt_count,
                lease_owner, lease_expires_at, started_at
            )
            VALUES ('content_hash', ?, 'running', 1, 'w', datetime('now', '+10 minute'), datetime('now'))
            RETURNING id
            "#,
        )
        .bind(rule_id)
        .fetch_one(&pool)
        .await
        .expect("running");

        let err = assert_no_running_reindex(&pool).await.expect_err("blocked");
        match err {
            CliError::MigrateBlockedByRunningReindex { count, job_ids } => {
                assert_eq!(count, 2);
                let mut sorted = job_ids;
                sorted.sort();
                let mut expected = vec![pending_id, running_id];
                expected.sort();
                assert_eq!(sorted, expected);
            }
            other => panic!("expected block, got {other:?}"),
        }
    }
}

async fn open_pool(app: &config::AppConfig) -> Result<sqlx::SqlitePool, CliError> {
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    build_sqlite_pool(
        &app.database.sqlite_path,
        app.database.max_connections,
        busy_timeout_ms,
    )
    .await
    .map_err(CliError::Storage)
}

async fn summary(action: &str, pool: &sqlx::SqlitePool) -> Result<MigrateCommandSummary, CliError> {
    let applied_versions =
        match sqlx::query_scalar::<_, i64>("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
        {
            Ok(values) => values,
            Err(sqlx::Error::Database(error)) if error.message().contains("_sqlx_migrations") => {
                Vec::new()
            }
            Err(error) => return Err(rss_ai_news_storage::StorageError::from(error).into()),
        };
    let current_version = applied_versions.iter().copied().max();
    Ok(MigrateCommandSummary {
        action: action.to_string(),
        applied_versions,
        current_version,
    })
}
