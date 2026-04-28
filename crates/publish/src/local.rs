use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use rss_ai_news_domain::dto::publish::RenderedReport;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::error::PublishError;
use crate::target::{PublishTarget, PublishedArtifact};

#[derive(Debug, Clone)]
pub struct LocalFsTarget {
    base_dir: PathBuf,
}

impl LocalFsTarget {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }
}

#[async_trait]
impl PublishTarget for LocalFsTarget {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        let relative = Path::new(&report.relative_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(PublishError::InvalidPath(report.relative_path.clone()));
        }

        let absolute = self.base_dir.join(relative);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut file = fs::File::create(&absolute).await?;
        file.write_all(report.markdown_content.as_bytes()).await?;
        file.flush().await?;

        Ok(PublishedArtifact {
            local_path: Some(absolute.to_string_lossy().into_owned()),
            commit_sha: None,
            remote_target: None,
        })
    }
}
