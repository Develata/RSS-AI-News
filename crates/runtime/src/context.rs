//! Explicit dependencies at each flow boundary. CLI owns construction and activation.
use std::sync::Arc;

use rss_ai_news_ai::AiClient;
use rss_ai_news_config::{
    ArtifactConfig, HttpConfig, LeaseConfig, PublishTemplateConfig, RetryConfig,
};
use rss_ai_news_extractor::{ContentStrategy, HtmlFetcher};
use rss_ai_news_feed::FeedFetcher;
use rss_ai_news_publish::PublishTarget;
use rss_ai_news_storage::{
    ArticleAiResultRepository, ArticleRepository, FeedEntryRepository, FeedSourceRepository,
    PublishItemRepository, PublishRecordRepository, RawArtifactRepository, ReindexJobRepository,
    RuleVersionRepository, RunEventRepository,
};
use time::OffsetDateTime;
use ulid::Ulid;

#[derive(Debug, Clone)]
pub struct RunMeta {
    pub run_id: String,
    pub started_at: OffsetDateTime,
}

impl Default for RunMeta {
    fn default() -> Self {
        Self {
            run_id: Ulid::new().to_string(),
            started_at: OffsetDateTime::now_utc(),
        }
    }
}

pub struct IngestDeps {
    pub run: RunMeta,
    pub http: HttpConfig,
    pub artifact: ArtifactConfig,
    pub feed_fetcher: Arc<dyn FeedFetcher>,
    pub feed_source_repo: Arc<dyn FeedSourceRepository>,
    pub feed_entry_repo: Arc<dyn FeedEntryRepository>,
    pub artifact_repo: Arc<dyn RawArtifactRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
    pub rule_version_repo: Arc<dyn RuleVersionRepository>,
}

pub struct ExtractDeps {
    pub run: RunMeta,
    pub http: HttpConfig,
    pub lease: LeaseConfig,
    pub retry: RetryConfig,
    pub artifact: ArtifactConfig,
    pub min_body_chars: u32,
    pub html_fetcher: Arc<dyn HtmlFetcher>,
    pub strategies: Vec<Arc<dyn ContentStrategy>>,
    pub feed_entry_repo: Arc<dyn FeedEntryRepository>,
    pub article_repo: Arc<dyn ArticleRepository>,
    pub artifact_repo: Arc<dyn RawArtifactRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
}

pub struct AiDeps {
    pub run: RunMeta,
    pub http: HttpConfig,
    pub lease: LeaseConfig,
    pub artifact: ArtifactConfig,
    pub ai_client: Arc<dyn AiClient>,
    pub article_repo: Arc<dyn ArticleRepository>,
    pub ai_result_repo: Arc<dyn ArticleAiResultRepository>,
    pub artifact_repo: Arc<dyn RawArtifactRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
}

pub struct PublishDeps {
    pub run: RunMeta,
    pub lease: LeaseConfig,
    pub retry: RetryConfig,
    pub template: PublishTemplateConfig,
    pub publish_target_local: Arc<dyn PublishTarget>,
    pub publish_target_remote: Option<Arc<dyn PublishTarget>>,
    pub publish_record_repo: Arc<dyn PublishRecordRepository>,
    pub publish_item_repo: Arc<dyn PublishItemRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
}

/// Snapshot readers and templates only; rebuilding cannot access a publisher.
pub struct RebuildReportDeps {
    pub template: PublishTemplateConfig,
    pub publish_record_repo: Arc<dyn PublishRecordRepository>,
    pub publish_item_repo: Arc<dyn PublishItemRepository>,
}

pub struct BackfillDeps {
    pub run: RunMeta,
    pub feed_entry_repo: Arc<dyn FeedEntryRepository>,
    pub article_repo: Arc<dyn ArticleRepository>,
    pub ai_result_repo: Arc<dyn ArticleAiResultRepository>,
    pub rule_version_repo: Arc<dyn RuleVersionRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
}

pub struct ReindexDeps {
    pub run: RunMeta,
    pub lease: LeaseConfig,
    pub feed_source_repo: Arc<dyn FeedSourceRepository>,
    pub feed_entry_repo: Arc<dyn FeedEntryRepository>,
    pub article_repo: Arc<dyn ArticleRepository>,
    pub rule_version_repo: Arc<dyn RuleVersionRepository>,
    pub reindex_job_repo: Arc<dyn ReindexJobRepository>,
    pub event_repo: Arc<dyn RunEventRepository>,
}
