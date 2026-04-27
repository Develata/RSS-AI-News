use std::sync::Arc;

use rss_ai_news_config::AppConfig;
use rss_ai_news_extractor::{ContentStrategy, HtmlFetcher};
use rss_ai_news_feed::FeedFetcher;
use rss_ai_news_storage::{
    ArticleRepository, FeedEntryRepository, FeedSourceRepository, RawArtifactRepository,
    RunEventRepository,
};
use time::OffsetDateTime;
use ulid::Ulid;

pub struct RunContext {
    pub run_id: String,
    pub started_at: OffsetDateTime,
    pub stage: String,
    pub app: Arc<AppConfig>,
    pub feed_fetcher: Arc<dyn FeedFetcher>,
    /// Extract-stage dependencies are required in the context even when the
    /// current flow is ingest-only; tests and CLI constructors should inject
    /// dummy values when a flow does not call them.
    pub html_fetcher: Arc<dyn HtmlFetcher>,
    pub strategies: Vec<Arc<dyn ContentStrategy>>,
    pub feed_source_repo: Arc<dyn FeedSourceRepository>,
    pub feed_entry_repo: Arc<dyn FeedEntryRepository>,
    pub article_repo: Arc<dyn ArticleRepository>,
    pub artifact_repo: Arc<dyn RawArtifactRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
}

pub struct RunContextDeps {
    pub feed_fetcher: Arc<dyn FeedFetcher>,
    pub html_fetcher: Arc<dyn HtmlFetcher>,
    pub strategies: Vec<Arc<dyn ContentStrategy>>,
    pub feed_source_repo: Arc<dyn FeedSourceRepository>,
    pub feed_entry_repo: Arc<dyn FeedEntryRepository>,
    pub article_repo: Arc<dyn ArticleRepository>,
    pub artifact_repo: Arc<dyn RawArtifactRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
}

impl RunContext {
    pub fn new_for_stage(stage: &str, app: Arc<AppConfig>, deps: RunContextDeps) -> Self {
        Self {
            run_id: Ulid::new().to_string(),
            started_at: OffsetDateTime::now_utc(),
            stage: stage.to_string(),
            app,
            feed_fetcher: deps.feed_fetcher,
            html_fetcher: deps.html_fetcher,
            strategies: deps.strategies,
            feed_source_repo: deps.feed_source_repo,
            feed_entry_repo: deps.feed_entry_repo,
            article_repo: deps.article_repo,
            artifact_repo: deps.artifact_repo,
            event_repo: deps.event_repo,
        }
    }
}
