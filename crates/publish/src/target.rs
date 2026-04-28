use async_trait::async_trait;
use rss_ai_news_domain::dto::publish::RenderedReport;

use crate::error::PublishError;

#[derive(Debug, Clone)]
pub struct PublishedArtifact {
    pub local_path: Option<String>,
    pub commit_sha: Option<String>,
    pub remote_target: Option<String>,
}

#[async_trait]
pub trait PublishTarget: Send + Sync {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError>;
}
