use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::Client;
use rss_ai_news_ai::{AiClient, AiClientConfig, AiError, AiResponse, AiTask, OpenAiCompatClient};
use rss_ai_news_config::{self as config, DatabaseDriver, LoadedConfig};
use rss_ai_news_domain::SecretString;
use rss_ai_news_extractor::{ContentStrategy, ReqwestHtmlFetcher};
use rss_ai_news_feed::ReqwestFeedFetcher;
use rss_ai_news_publish::{GitHubTarget, GitHubTargetConfig, LocalFsTarget, PublishTarget};
use rss_ai_news_runtime::{RunContext, RunContextDeps};
use rss_ai_news_storage::{
    ArticleAiResultRepo, ArticleRepo, FeedEntryRepo, FeedSourceRepo, PublishItemRepo,
    PublishRecordRepo, RawArtifactRepo, ReindexJobRepo, RuleVersionRepo, RuleVersionRepository,
    RunEventRepo, StorageError, StoragePool, build_sqlite_pool, run_migrations,
};
use sqlx::SqlitePool;

use crate::{db_url::resolve_storage_url, error::CliError};

/// W11-P4-C：cli/runtime PG 端到端入口。
///
/// 按 [`docs/design/storage-multi-dialect.md`] §5.4 通过 [`resolve_storage_url`]
/// 解析 `driver` + `DATABASE_URL`，[`StoragePool::build`] 按 URL scheme 路由到
/// `StoragePool::{Sqlite, Postgres}`。所有 10 个 repo 通过
/// `new_with_storage(StoragePool)` 入口注入，业务方法内部按 backend `match`
/// 分发（P3-C/E 已实装）。
///
/// 返回值不再含 pool（原 `_pool` 在 7 个调用点均未使用），让签名直接反映
/// "这里只构造 ctx" 的语义。
pub async fn build_run_context(
    stage: &str,
    loaded: &LoadedConfig,
) -> Result<Arc<RunContext>, CliError> {
    let app = Arc::new(loaded.app.clone());
    let url = resolve_storage_url(loaded)?;
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = StoragePool::build(&url, app.database.max_connections, busy_timeout_ms)
        .await
        .map_err(CliError::Storage)?;
    run_migrations(&pool).await.map_err(CliError::Storage)?;
    ensure_default_rule_version(&pool, &loaded.config_sha256)
        .await
        .map_err(CliError::Storage)?;

    let feed_fetcher = Arc::new(ReqwestFeedFetcher::new(app.extractor.max_body_bytes)?);
    let html_fetcher = Arc::new(ReqwestHtmlFetcher::new(app.extractor.max_body_bytes)?);
    let strategies: Vec<Arc<dyn ContentStrategy>> = Vec::new();

    let ai_client: Arc<dyn AiClient> = if app.ai.enabled
        && loaded
            .env
            .openai_api_key
            .as_ref()
            .map(SecretString::expose_secret)
            .is_some_and(|value| !value.trim().is_empty())
    {
        // Pass the SecretString through end-to-end (W2-A2). The branch
        // condition above already verified `openai_api_key` is `Some(_)`
        // and non-empty after trim, so cloning the original is sound.
        let api_key = loaded
            .env
            .openai_api_key
            .clone()
            .unwrap_or_else(|| SecretString::from(""));
        Arc::new(OpenAiCompatClient::new(AiClientConfig {
            api_base: loaded.env.openai_base_url.clone().unwrap_or_default(),
            api_key,
            request_timeout: Duration::from_secs(app.ai.request_timeout_seconds),
        })?)
    } else {
        Arc::new(NullAiClient)
    };

    let publish_target_local: Arc<dyn PublishTarget> =
        Arc::new(LocalFsTarget::new(app.publish.local_output_dir.clone()));
    let publish_target_remote: Option<Arc<dyn PublishTarget>> =
        if !app.publish.github_owner.trim().is_empty()
            && !app.publish.github_repo.trim().is_empty()
            && loaded
                .env
                .github_token
                .as_ref()
                .map(SecretString::expose_secret)
                .is_some_and(|value| !value.trim().is_empty())
        {
            // Same pattern as the AI api_key above: the surrounding `if`
            // already ensured `github_token` is `Some(_)` and non-empty,
            // so we forward the SecretString unchanged (W2-A2).
            let token = loaded
                .env
                .github_token
                .clone()
                .unwrap_or_else(|| SecretString::from(""));
            Some(Arc::new(GitHubTarget::new(GitHubTargetConfig {
                token,
                owner: app.publish.github_owner.clone(),
                repo: app.publish.github_repo.clone(),
                branch: app.publish.github_branch.clone(),
                path_prefix: app.publish.github_path_prefix.clone(),
                commit_message_prefix: "rss-ai-news".to_string(),
            })?))
        } else {
            None
        };

    let ctx = RunContext::new_for_stage(
        stage,
        app,
        RunContextDeps {
            feed_fetcher,
            html_fetcher,
            strategies,
            ai_client,
            publish_target_local,
            publish_target_remote,
            feed_source_repo: Arc::new(FeedSourceRepo::new_with_storage(pool.clone())),
            feed_entry_repo: Arc::new(FeedEntryRepo::new_with_storage(pool.clone())),
            article_repo: Arc::new(ArticleRepo::new_with_storage(pool.clone())),
            ai_result_repo: Arc::new(ArticleAiResultRepo::new_with_storage(pool.clone())),
            publish_record_repo: Arc::new(PublishRecordRepo::new_with_storage(pool.clone())),
            publish_item_repo: Arc::new(PublishItemRepo::new_with_storage(pool.clone())),
            artifact_repo: Arc::new(RawArtifactRepo::new_with_storage(pool.clone())),
            event_repo: Arc::new(RunEventRepo::new_with_storage(pool.clone())),
            rule_version_repo: Arc::new(RuleVersionRepo::new_with_storage(pool.clone())),
            reindex_job_repo: Arc::new(ReindexJobRepo::new_with_storage(pool)),
        },
    );

    Ok(Arc::new(ctx))
}

pub struct ReplayDeps {
    pub pool: SqlitePool,
    pub artifact_repo: Arc<dyn rss_ai_news_storage::RawArtifactRepository>,
    pub article_repo: Arc<dyn rss_ai_news_storage::ArticleRepository>,
    pub feed_entry_repo: Arc<dyn rss_ai_news_storage::FeedEntryRepository>,
}

pub async fn build_replay_deps(cli: &crate::args::Cli) -> Result<ReplayDeps, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    require_sqlite_driver(&loaded)?;
    let app = &loaded.app;
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = build_sqlite_pool(
        &app.database.sqlite_path,
        app.database.max_connections,
        busy_timeout_ms,
    )
    .await
    .map_err(CliError::Storage)?;
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .map_err(CliError::Storage)?;

    Ok(ReplayDeps {
        pool: pool.clone(),
        artifact_repo: Arc::new(RawArtifactRepo::new(pool.clone())),
        article_repo: Arc::new(ArticleRepo::new(pool.clone())),
        feed_entry_repo: Arc::new(FeedEntryRepo::new(pool)),
    })
}

pub struct DoctorDeps {
    pub loaded: Arc<LoadedConfig>,
    pub pool: SqlitePool,
    pub http_client: Client,
}

pub async fn build_doctor_deps(cli: &crate::args::Cli) -> Result<DoctorDeps, CliError> {
    let loaded = Arc::new(config::load(&cli.config_dir, None, cli.to_cli_overrides())?);
    require_sqlite_driver(&loaded)?;
    let app = &loaded.app;
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = build_sqlite_pool(
        &app.database.sqlite_path,
        app.database.max_connections,
        busy_timeout_ms,
    )
    .await
    .map_err(CliError::Storage)?;
    let storage = StoragePool::Sqlite(pool.clone());
    run_migrations(&storage).await.map_err(CliError::Storage)?;
    ensure_default_rule_version(&storage, &loaded.config_sha256)
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

/// W11-P1-E：废除原硬编码 `INSERT OR IGNORE INTO rule_versions (id, ...) VALUES (1, ...)`。
/// 显式 id=1 在 PG `GENERATED BY DEFAULT AS IDENTITY` 下不推进 sequence，
/// 后续隐式 INSERT 会再次生成 id=1 触发主键冲突；改 `ON CONFLICT(id) DO NOTHING`
/// 也会在 id=1 已被其它 `kind` 占用时静默跳过，语义错误。
///
/// 改走 F15-fix6 已有的 [`RuleVersionRepository::active_rule_or_register`] 路径：
/// 生产场景读现有 `kind='config'` active 行；首次部署 / 测试 fixture seed
/// 一个 active 首版，tag 显式标 `cli-default`。详见 storage-multi-dialect §2.5。
async fn ensure_default_rule_version(
    pool: &StoragePool,
    config_sha256: &str,
) -> Result<(), rss_ai_news_storage::StorageError> {
    let repo = RuleVersionRepo::new_with_storage(pool.clone());
    repo.active_rule_or_register(
        "config",
        "cli-default",
        "CLI default runtime rule version",
        config_sha256,
    )
    .await?;
    Ok(())
}

/// W11-P3-A-fix1.H1：driver=postgres 时拒绝 build_*_deps，引导用户走 cli migrate
/// 或回退 sqlite。仅 `cli migrate` 子命令对 PG 放行（参见 commands/migrate.rs）。
fn require_sqlite_driver(loaded: &LoadedConfig) -> Result<(), CliError> {
    if loaded.app.database.driver == DatabaseDriver::Postgres {
        return Err(CliError::Storage(StorageError::UnsupportedBackend(
            "postgres repo path is P3+; only `cli migrate` may currently target postgres".into(),
        )));
    }
    Ok(())
}

struct NullAiClient;

#[async_trait]
impl AiClient for NullAiClient {
    async fn invoke(&self, _task: &AiTask) -> Result<AiResponse, AiError> {
        Err(AiError::ConnectionFailed(
            "ai client not configured".to_string(),
        ))
    }
}
