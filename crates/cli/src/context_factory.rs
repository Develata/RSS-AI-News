use crate::{db_url::resolve_storage_url, error::CliError};
use reqwest::Client;
use rss_ai_news_ai::{AiClientConfig, OpenAiCompatClient};
use rss_ai_news_config::{self as config, AiCredentials, LoadedConfig};
use rss_ai_news_extractor::{ContentStrategy, ReadabilityStrategy, ReqwestHtmlFetcher};
use rss_ai_news_feed::ReqwestFeedFetcher;
use rss_ai_news_publish::{GitHubTarget, GitHubTargetConfig, LocalFsTarget, PublishTarget};
use rss_ai_news_runtime::{
    AiDeps, BackfillDeps, ExtractDeps, IngestDeps, PublishDeps, RebuildReportDeps,
    RecentEntriesFlow, ReindexDeps, RunMeta,
};
use rss_ai_news_storage::{
    ArticleAiResultRepo, ArticleRepo, ConfigRotation, FeedEntryRepo, FeedSourceRepo,
    PublishItemRepo, PublishRecordRepo, RawArtifactRepo, ReindexJobRepo, RuleVersionRepo,
    RunEventRepo, StoragePool, applied_migration_versions, ensure_migration_state_exact,
    pending_migration_versions, run_migrations,
};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;

/// Only write commands migrate and rotate the active configuration.
pub async fn open_write_storage(loaded: &LoadedConfig) -> Result<StoragePool, CliError> {
    let app = &loaded.app;
    let url = resolve_storage_url(loaded)?;
    let pool = StoragePool::build(
        &url,
        app.database.max_connections,
        u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX),
    )
    .await?;
    run_migrations(&pool).await?;
    ensure_active_config_version(&pool, &loaded.config_sha256).await?;
    Ok(pool)
}

pub async fn open_read_storage(loaded: &LoadedConfig) -> Result<StoragePool, CliError> {
    let url = resolve_storage_url(loaded)?;
    let pool = StoragePool::build_read_only(
        &url,
        u32::try_from(loaded.app.database.busy_timeout_ms).unwrap_or(u32::MAX),
    )
    .await?;
    ensure_migration_state_exact(&pool).await?;
    Ok(pool)
}

pub fn build_ingest_deps(
    loaded: &LoadedConfig,
    pool: &StoragePool,
) -> Result<Arc<IngestDeps>, CliError> {
    let app = &loaded.app;
    Ok(Arc::new(IngestDeps {
        run: RunMeta::default(),
        http: app.http.clone(),
        artifact: app.artifact.clone(),
        feed_fetcher: Arc::new(ReqwestFeedFetcher::new(
            app.extractor.effective_feed_max_body_bytes(),
        )?),
        feed_source_repo: Arc::new(FeedSourceRepo::new_with_storage(pool.clone())),
        feed_entry_repo: Arc::new(FeedEntryRepo::new_with_storage(pool.clone())),
        artifact_repo: Arc::new(RawArtifactRepo::new_with_storage(pool.clone())),
        event_repo: Arc::new(RunEventRepo::new_with_storage(pool.clone())),
        rule_version_repo: Arc::new(RuleVersionRepo::new_with_storage(pool.clone())),
    }))
}

pub fn build_extract_deps(
    loaded: &LoadedConfig,
    pool: &StoragePool,
    run: RunMeta,
) -> Result<Arc<ExtractDeps>, CliError> {
    let app = &loaded.app;
    let strategies: Vec<Arc<dyn ContentStrategy>> = app
        .extractor
        .strategy_order
        .iter()
        .filter(|name| name.as_str() == "readability")
        .map(|_| Arc::new(ReadabilityStrategy) as Arc<dyn ContentStrategy>)
        .collect();
    Ok(Arc::new(ExtractDeps {
        run,
        http: app.http.clone(),
        lease: app.lease.clone(),
        retry: app.retry.clone(),
        artifact: app.artifact.clone(),
        min_body_chars: app.extractor.min_body_chars,
        html_fetcher: Arc::new(ReqwestHtmlFetcher::new(app.extractor.max_body_bytes)?),
        strategies,
        feed_entry_repo: Arc::new(FeedEntryRepo::new_with_storage(pool.clone())),
        article_repo: Arc::new(ArticleRepo::new_with_storage(pool.clone())),
        artifact_repo: Arc::new(RawArtifactRepo::new_with_storage(pool.clone())),
        event_repo: Arc::new(RunEventRepo::new_with_storage(pool.clone())),
    }))
}

pub fn build_ai_deps(
    loaded: &LoadedConfig,
    pool: &StoragePool,
    credentials: AiCredentials,
) -> Result<Arc<AiDeps>, CliError> {
    let app = &loaded.app;
    Ok(Arc::new(AiDeps {
        run: RunMeta::default(),
        http: app.http.clone(),
        lease: app.lease.clone(),
        artifact: app.artifact.clone(),
        ai_client: Arc::new(OpenAiCompatClient::new(AiClientConfig {
            api_base: credentials.base_url,
            api_key: credentials.api_key,
            request_timeout: Duration::from_secs(app.ai.request_timeout_seconds),
        })?),
        article_repo: Arc::new(ArticleRepo::new_with_storage(pool.clone())),
        ai_result_repo: Arc::new(ArticleAiResultRepo::new_with_storage(pool.clone())),
        artifact_repo: Arc::new(RawArtifactRepo::new_with_storage(pool.clone())),
        event_repo: Arc::new(RunEventRepo::new_with_storage(pool.clone())),
    }))
}

pub fn build_publish_deps(
    loaded: &LoadedConfig,
    pool: &StoragePool,
    local_only: bool,
) -> Result<Arc<PublishDeps>, CliError> {
    let app = &loaded.app;
    let publish_target_remote: Option<Arc<dyn PublishTarget>> = if !local_only
        && !app.publish.github_owner.trim().is_empty()
        && !app.publish.github_repo.trim().is_empty()
    {
        loaded
            .env
            .github_token
            .as_ref()
            .filter(|token| !token.expose_secret().trim().is_empty())
            .map(|token| {
                GitHubTarget::new(GitHubTargetConfig {
                    token: token.clone(),
                    owner: app.publish.github_owner.clone(),
                    repo: app.publish.github_repo.clone(),
                    branch: app.publish.github_branch.clone(),
                    path_prefix: app.publish.github_path_prefix.clone(),
                    commit_message_prefix: "rss-ai-news".into(),
                })
                .map(|target| Arc::new(target) as Arc<dyn PublishTarget>)
            })
            .transpose()?
    } else {
        None
    };
    Ok(Arc::new(PublishDeps {
        run: RunMeta::default(),
        lease: app.lease.clone(),
        retry: app.retry.clone(),
        template: app.publish.template.clone(),
        publish_target_local: Arc::new(LocalFsTarget::new(app.publish.local_output_dir.clone())),
        publish_target_remote,
        publish_record_repo: Arc::new(PublishRecordRepo::new_with_storage(pool.clone())),
        publish_item_repo: Arc::new(PublishItemRepo::new_with_storage(pool.clone())),
        event_repo: Arc::new(RunEventRepo::new_with_storage(pool.clone())),
    }))
}

pub fn build_rebuild_report_deps(
    loaded: &LoadedConfig,
    pool: &StoragePool,
) -> Result<Arc<RebuildReportDeps>, CliError> {
    let app = &loaded.app;
    Ok(Arc::new(RebuildReportDeps {
        template: app.publish.template.clone(),
        publish_record_repo: Arc::new(PublishRecordRepo::new_with_storage(pool.clone())),
        publish_item_repo: Arc::new(PublishItemRepo::new_with_storage(pool.clone())),
    }))
}

pub fn build_backfill_deps(
    _loaded: &LoadedConfig,
    pool: &StoragePool,
) -> Result<Arc<BackfillDeps>, CliError> {
    Ok(Arc::new(BackfillDeps {
        run: RunMeta::default(),
        feed_entry_repo: Arc::new(FeedEntryRepo::new_with_storage(pool.clone())),
        article_repo: Arc::new(ArticleRepo::new_with_storage(pool.clone())),
        ai_result_repo: Arc::new(ArticleAiResultRepo::new_with_storage(pool.clone())),
        rule_version_repo: Arc::new(RuleVersionRepo::new_with_storage(pool.clone())),
        event_repo: Arc::new(RunEventRepo::new_with_storage(pool.clone())),
    }))
}

pub fn build_reindex_deps(
    loaded: &LoadedConfig,
    pool: &StoragePool,
) -> Result<Arc<ReindexDeps>, CliError> {
    let app = &loaded.app;
    Ok(Arc::new(ReindexDeps {
        run: RunMeta::default(),
        lease: app.lease.clone(),
        feed_source_repo: Arc::new(FeedSourceRepo::new_with_storage(pool.clone())),
        feed_entry_repo: Arc::new(FeedEntryRepo::new_with_storage(pool.clone())),
        article_repo: Arc::new(ArticleRepo::new_with_storage(pool.clone())),
        rule_version_repo: Arc::new(RuleVersionRepo::new_with_storage(pool.clone())),
        reindex_job_repo: Arc::new(ReindexJobRepo::new_with_storage(pool.clone())),
        event_repo: Arc::new(RunEventRepo::new_with_storage(pool.clone())),
    }))
}

/// 构造严格只读 recent-entries flow：不自动 migrate、不轮换 config version、
/// 不创建 HTTP/AI/publish clients，也不建立 writer repositories。
pub async fn build_recent_entries_flow(
    cli: &crate::args::Cli,
) -> Result<RecentEntriesFlow, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    let url = resolve_storage_url(&loaded)?;
    let busy_timeout_ms = u32::try_from(loaded.app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = StoragePool::build_read_only(&url, busy_timeout_ms)
        .await
        .map_err(CliError::Storage)?;

    let applied = applied_migration_versions(&pool)
        .await
        .map_err(CliError::Storage)?;
    let pending = pending_migration_versions(&pool, &applied);
    if !pending.is_empty() {
        return Err(CliError::RecentEntriesMigrationPending { pending });
    }
    ensure_migration_state_exact(&pool).await.map_err(|error| {
        CliError::RecentEntriesMigrationDrift {
            detail: error.to_string(),
        }
    })?;

    Ok(RecentEntriesFlow::new(
        Arc::new(FeedSourceRepo::new_with_storage(pool.clone())),
        Arc::new(FeedEntryRepo::new_with_storage(pool)),
    ))
}

pub struct ReplayDeps {
    /// W11-P4-C2：StoragePool 替代 SqlitePool；replay.rs 内部按 backend match。
    pub pool: StoragePool,
    pub artifact_repo: Arc<dyn rss_ai_news_storage::RawArtifactRepository>,
    pub article_repo: Arc<dyn rss_ai_news_storage::ArticleRepository>,
    pub feed_entry_repo: Arc<dyn rss_ai_news_storage::FeedEntryRepository>,
}

pub async fn build_replay_deps(cli: &crate::args::Cli) -> Result<ReplayDeps, CliError> {
    // W11-P4-C2：原 require_sqlite_driver 拦截已移除。replay 的 artifact /
    // article / feed_entry repo 在 P3-C/E 全部双轨化；html_diff SQL 已升 $1。
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    let app = &loaded.app;
    let url = resolve_storage_url(&loaded)?;
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = StoragePool::build_read_only(&url, busy_timeout_ms).await?;
    ensure_migration_state_exact(&pool).await?;

    Ok(ReplayDeps {
        pool: pool.clone(),
        artifact_repo: Arc::new(RawArtifactRepo::new_with_storage(pool.clone())),
        article_repo: Arc::new(ArticleRepo::new_with_storage(pool.clone())),
        feed_entry_repo: Arc::new(FeedEntryRepo::new_with_storage(pool)),
    })
}

pub struct DoctorDeps {
    pub loaded: Arc<LoadedConfig>,
    /// W11-P4-C2：StoragePool 替代 SqlitePool；4 个 health-check + deep_scan
    /// 在本期已全部双轨化。
    pub pool: StoragePool,
    pub http_client: Client,
}

pub async fn build_doctor_deps(cli: &crate::args::Cli) -> Result<DoctorDeps, CliError> {
    // W11-P4-C2：原 require_sqlite_driver 拦截已移除；observability::health.rs
    // 4 个 check（DatabaseConnectivity / MigrationVersion / ExpiredLease /
    // FailedBacklog）+ runtime::doctor::deep_scan 已全部接 &StoragePool。
    let loaded = Arc::new(config::load_skip_env_checks(
        &cli.config_dir,
        None,
        cli.to_cli_overrides(),
    )?);
    let app = &loaded.app;
    let url = resolve_storage_url(&loaded)?;
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = StoragePool::build(&url, app.database.max_connections, busy_timeout_ms)
        .await
        .map_err(CliError::Storage)?;
    let http_client = Client::builder()
        .timeout(Duration::from_secs(app.http.timeout_seconds.max(1)))
        .build()
        .map_err(|error| CliError::Io(std::io::Error::other(error)))?;

    Ok(DoctorDeps {
        loaded,
        pool,
        http_client,
    })
}

/// W16（docs/plan/16-config-versioning.md §5）：启动期让 `kind='config'` 的
/// active 行跟随当前真实 `config_sha256`。
///
/// 取代原 `ensure_default_rule_version`（W11-P1-E 引入）：旧实现只在"无
/// active"时 seed（tag 固定 `cli-default`），config 改动后新 sha 永不落库、
/// bootstrap placeholder 滞留 active（D1/D2）。现改为 sha-keyed 轮换：
/// sha 一致 → 单 SELECT 零写入；漂移 → 单事务 demote + 复用/插入 + promote。
/// 轮换只走 tracing 留痕——seed 在 flow 装配之前执行无 run_id，
/// rule_versions 行自身即审计记录。
async fn ensure_active_config_version(
    pool: &StoragePool,
    config_sha256: &str,
) -> Result<(), rss_ai_news_storage::StorageError> {
    let repo = RuleVersionRepo::new_with_storage(pool.clone());
    let rotation = repo
        .rotate_active_config(
            config_sha256,
            "CLI startup config snapshot",
            OffsetDateTime::now_utc(),
        )
        .await?;
    if let ConfigRotation::Rotated { new_id, demoted_id } = rotation {
        tracing::info!(
            new_id,
            demoted_id,
            sha_prefix = &config_sha256[..config_sha256.len().min(12)],
            "active config rotated to current config_sha256"
        );
    }
    Ok(())
}
