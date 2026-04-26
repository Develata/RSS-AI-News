use std::sync::Arc;

use rss_ai_news_config::AppConfig;
use rss_ai_news_feed::FeedFetcher;
use rss_ai_news_storage::{
    FeedEntryRepository, FeedSourceRepository, RawArtifactRepository, RunEventRepository,
};
use time::OffsetDateTime;
use ulid::Ulid;

pub struct RunContext {
    pub run_id: String,
    pub started_at: OffsetDateTime,
    pub stage: String,
    pub app: Arc<AppConfig>,
    pub feed_fetcher: Arc<dyn FeedFetcher>,
    pub feed_source_repo: Arc<dyn FeedSourceRepository>,
    pub feed_entry_repo: Arc<dyn FeedEntryRepository>,
    pub artifact_repo: Arc<dyn RawArtifactRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
}

impl RunContext {
    pub fn new_for_stage(
        stage: &str,
        app: Arc<AppConfig>,
        feed_fetcher: Arc<dyn FeedFetcher>,
        feed_source_repo: Arc<dyn FeedSourceRepository>,
        feed_entry_repo: Arc<dyn FeedEntryRepository>,
        artifact_repo: Arc<dyn RawArtifactRepository>,
        event_repo: Arc<dyn RunEventRepository>,
    ) -> Self {
        Self {
            run_id: Ulid::new().to_string(),
            started_at: OffsetDateTime::now_utc(),
            stage: stage.to_string(),
            app,
            feed_fetcher,
            feed_source_repo,
            feed_entry_repo,
            artifact_repo,
            event_repo,
        }
    }
}
