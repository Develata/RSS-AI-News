use std::sync::Arc;

use rss_ai_news_storage::{
    RecentFeedEntry, RecentFeedEntryFilter, RecentFeedEntryRepository, RecentFeedSourceHealth,
    RecentFeedSourceHealthRepository,
};
use time::OffsetDateTime;

use crate::error::RuntimeError;

pub const DEFAULT_RECENT_ENTRIES_LIMIT: u32 = 50;
pub const MAX_RECENT_ENTRIES_LIMIT: u32 = 200;
pub const MAX_RECENT_SOURCE_HEALTH_ROWS: u32 = 500;

/// 只读 projection 独立持有最小依赖，不复用包含 HTTP/AI/publish/writer repos 的
/// `RunContext`。这让 read-only side-effect boundary 在类型结构上可见。
pub struct RecentEntriesFlow {
    feed_source_health_repo: Arc<dyn RecentFeedSourceHealthRepository>,
    feed_entry_repo: Arc<dyn RecentFeedEntryRepository>,
}

#[derive(Debug, Clone)]
pub struct RecentEntriesOptions {
    pub category_key: String,
    pub discovered_after: OffsetDateTime,
    pub published_after: Option<OffsetDateTime>,
    pub limit: u32,
}

pub type RecentSourceHealth = RecentFeedSourceHealth;

#[derive(Debug, Clone)]
pub struct RecentEntriesResult {
    pub generated_at: OffsetDateTime,
    pub category: String,
    pub discovered_after: OffsetDateTime,
    pub published_after: Option<OffsetDateTime>,
    pub limit: u32,
    pub truncated: bool,
    pub source_health_truncated: bool,
    pub source_health: Vec<RecentSourceHealth>,
    pub entries: Vec<RecentFeedEntry>,
}

impl RecentEntriesFlow {
    pub fn new(
        feed_source_health_repo: Arc<dyn RecentFeedSourceHealthRepository>,
        feed_entry_repo: Arc<dyn RecentFeedEntryRepository>,
    ) -> Self {
        Self {
            feed_source_health_repo,
            feed_entry_repo,
        }
    }

    pub async fn execute(
        &self,
        options: RecentEntriesOptions,
    ) -> Result<RecentEntriesResult, RuntimeError> {
        if !(1..=MAX_RECENT_ENTRIES_LIMIT).contains(&options.limit) {
            return Err(RuntimeError::Config(format!(
                "recent-entries limit must be in 1..={MAX_RECENT_ENTRIES_LIMIT}, got {}",
                options.limit
            )));
        }

        let mut source_health = self
            .feed_source_health_repo
            .list_recent_health(&options.category_key, MAX_RECENT_SOURCE_HEALTH_ROWS + 1)
            .await?;
        let source_health_truncated = source_health.len() > MAX_RECENT_SOURCE_HEALTH_ROWS as usize;
        source_health.truncate(MAX_RECENT_SOURCE_HEALTH_ROWS as usize);
        let mut entries = self
            .feed_entry_repo
            .list_recent(&RecentFeedEntryFilter {
                category_key: options.category_key.clone(),
                discovered_after: options.discovered_after,
                published_after: options.published_after,
                max_rows: options.limit + 1,
            })
            .await?;
        let truncated = entries.len() > options.limit as usize;
        entries.truncate(options.limit as usize);

        Ok(RecentEntriesResult {
            generated_at: OffsetDateTime::now_utc(),
            category: options.category_key,
            discovered_after: options.discovered_after,
            published_after: options.published_after,
            limit: options.limit,
            truncated,
            source_health_truncated,
            source_health,
            entries,
        })
    }
}
