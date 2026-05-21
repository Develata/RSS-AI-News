use async_trait::async_trait;
use rss_ai_news_domain::dto::publish::RenderedReport;

use crate::error::PublishError;

#[derive(Debug, Clone)]
pub struct PublishedArtifact {
    pub local_path: Option<String>,
    pub commit_sha: Option<String>,
    pub remote_target: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublishedBatchArtifact {
    pub artifacts: Vec<PublishedArtifact>,
    pub commit_sha: Option<String>,
}

#[async_trait]
pub trait PublishTarget: Send + Sync {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError>;

    async fn publish_many(
        &self,
        reports: &[RenderedReport],
    ) -> Result<PublishedBatchArtifact, PublishError> {
        let mut artifacts = Vec::with_capacity(reports.len());
        let mut commit_sha = None;
        for report in reports {
            let artifact = self.publish(report).await?;
            if artifact.commit_sha.is_some() {
                commit_sha = artifact.commit_sha.clone();
            }
            artifacts.push(artifact);
        }
        Ok(PublishedBatchArtifact {
            artifacts,
            commit_sha,
        })
    }
}
