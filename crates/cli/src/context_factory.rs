use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::Client;
use rss_ai_news_ai::{AiClient, AiClientConfig, AiError, AiResponse, AiTask, OpenAiCompatClient};
use rss_ai_news_config::{self as config, LoadedConfig};
use rss_ai_news_domain::SecretString;
use rss_ai_news_extractor::{ContentStrategy, ReqwestHtmlFetcher};
use rss_ai_news_feed::ReqwestFeedFetcher;
use rss_ai_news_publish::{GitHubTarget, GitHubTargetConfig, LocalFsTarget, PublishTarget};
use rss_ai_news_runtime::{RunContext, RunContextDeps};
use rss_ai_news_storage::{
    SqliteArticleAiResultRepo, SqliteArticleRepo, SqliteFeedEntryRepo, SqliteFeedSourceRepo,
    SqlitePublishItemRepo, SqlitePublishRecordRepo, SqliteRawArtifactRepo, SqliteRuleVersionRepo,
    SqliteRunEventRepo, build_sqlite_pool, run_migrations,
};
use sqlx::SqlitePool;

use crate::error::CliError;

pub async fn build_run_context(
    stage: &str,
    loaded: &LoadedConfig,
) -> Result<(SqlitePool, Arc<RunContext>), CliError> {
    let app = Arc::new(loaded.app.clone());
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = build_sqlite_pool(
        &app.database.sqlite_path,
        app.database.max_connections,
        busy_timeout_ms,
    )
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
        Arc::new(OpenAiCompatClient::new(AiClientConfig {
            api_base: loaded.env.openai_base_url.clone().unwrap_or_default(),
            api_key: loaded
                .env
                .openai_api_key
                .as_ref()
                .map(|secret| secret.expose_secret().to_owned())
                .unwrap_or_default(),
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
            Some(Arc::new(GitHubTarget::new(GitHubTargetConfig {
                token: loaded
                    .env
                    .github_token
                    .as_ref()
                    .map(|secret| secret.expose_secret().to_owned())
                    .unwrap_or_default(),
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
            feed_source_repo: Arc::new(SqliteFeedSourceRepo::new(pool.clone())),
            feed_entry_repo: Arc::new(SqliteFeedEntryRepo::new(pool.clone())),
            article_repo: Arc::new(SqliteArticleRepo::new(pool.clone())),
            ai_result_repo: Arc::new(SqliteArticleAiResultRepo::new(pool.clone())),
            publish_record_repo: Arc::new(SqlitePublishRecordRepo::new(pool.clone())),
            publish_item_repo: Arc::new(SqlitePublishItemRepo::new(pool.clone())),
            artifact_repo: Arc::new(SqliteRawArtifactRepo::new(pool.clone())),
            event_repo: Arc::new(SqliteRunEventRepo::new(pool.clone())),
            rule_version_repo: Arc::new(SqliteRuleVersionRepo::new(pool.clone())),
        },
    );

    Ok((pool, Arc::new(ctx)))
}

pub struct ReplayDeps {
    pub pool: SqlitePool,
    pub artifact_repo: Arc<dyn rss_ai_news_storage::RawArtifactRepository>,
    pub article_repo: Arc<dyn rss_ai_news_storage::ArticleRepository>,
    pub feed_entry_repo: Arc<dyn rss_ai_news_storage::FeedEntryRepository>,
}

pub async fn build_replay_deps(cli: &crate::args::Cli) -> Result<ReplayDeps, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let app = &loaded.app;
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = build_sqlite_pool(
        &app.database.sqlite_path,
        app.database.max_connections,
        busy_timeout_ms,
    )
    .await
    .map_err(CliError::Storage)?;
    run_migrations(&pool).await.map_err(CliError::Storage)?;

    Ok(ReplayDeps {
        pool: pool.clone(),
        artifact_repo: Arc::new(SqliteRawArtifactRepo::new(pool.clone())),
        article_repo: Arc::new(SqliteArticleRepo::new(pool.clone())),
        feed_entry_repo: Arc::new(SqliteFeedEntryRepo::new(pool)),
    })
}

pub struct DoctorDeps {
    pub loaded: Arc<LoadedConfig>,
    pub pool: SqlitePool,
    pub http_client: Client,
}

pub async fn build_doctor_deps(cli: &crate::args::Cli) -> Result<DoctorDeps, CliError> {
    let loaded = Arc::new(config::load(&cli.config_dir, None, cli.to_cli_overrides())?);
    let app = &loaded.app;
    let busy_timeout_ms = u32::try_from(app.database.busy_timeout_ms).unwrap_or(u32::MAX);
    let pool = build_sqlite_pool(
        &app.database.sqlite_path,
        app.database.max_connections,
        busy_timeout_ms,
    )
    .await
    .map_err(CliError::Storage)?;
    run_migrations(&pool).await.map_err(CliError::Storage)?;
    ensure_default_rule_version(&pool, &loaded.config_sha256)
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

async fn ensure_default_rule_version(
    pool: &SqlitePool,
    config_sha256: &str,
) -> Result<(), rss_ai_news_storage::StorageError> {
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO rule_versions (id, kind, version_tag, description, payload_sha256)
        VALUES (1, 'config', 'cli-default', 'CLI default runtime rule version', ?)
        "#,
    )
    .bind(config_sha256)
    .execute(pool)
    .await
    .map_err(rss_ai_news_storage::StorageError::from)?;
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
