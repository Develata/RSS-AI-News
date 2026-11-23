use async_trait::async_trait;
use rss_ai_news_domain::dto::publish::{RemoteBlob, RenderedReport};

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

/// R5-9: asked by a target right before each request that changes the
/// destination (a contents PUT, a branch ref PATCH), after the reads the
/// request is conditional on. `Err` = do not send: the caller no longer owns
/// the write (another worker took it over) or cannot tell. The target returns
/// that error and sends nothing more for the call.
#[async_trait]
pub trait WriteGuard: Send + Sync {
    async fn confirm(&self) -> Result<(), PublishError>;
}

#[async_trait]
pub trait PublishTarget: Send + Sync {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError>;

    /// [`Self::publish`], with `guard` confirmed right before the mutating
    /// request. The default confirms once before calling [`Self::publish`];
    /// a target with its own reads before the write (GitHub) confirms after
    /// them instead.
    async fn publish_guarded(
        &self,
        report: &RenderedReport,
        guard: &dyn WriteGuard,
    ) -> Result<PublishedArtifact, PublishError> {
        guard.confirm().await?;
        self.publish(report).await
    }

    /// [`Self::publish_many`] with `guard`, as [`Self::publish_guarded`].
    async fn publish_many_guarded(
        &self,
        reports: &[RenderedReport],
        guard: &dyn WriteGuard,
    ) -> Result<PublishedBatchArtifact, PublishError> {
        guard.confirm().await?;
        self.publish_many(reports).await
    }

    /// Stable identity of the destination this target writes to, in the form
    /// stored in `publish_records.remote_target` at init (`owner/repo:branch`
    /// for GitHub). The remote stage refuses to send a record whose stored
    /// identity differs. `None` = the target declares no identity (local
    /// targets, test doubles); no comparison is made.
    fn target_identity(&self) -> Option<String> {
        None
    }

    /// Read-only receipt lookup after an attempt whose outcome is unknown
    /// (the write was cancelled by the deadline, or its response was lost):
    /// `Some` with one artifact per report, in order, iff the destination
    /// already holds every report byte for byte; `None` = not there, or the
    /// target cannot tell (the default). Never writes.
    async fn find_published(
        &self,
        _reports: &[RenderedReport],
    ) -> Result<Option<PublishedBatchArtifact>, PublishError> {
        Ok(None)
    }

    /// R5-2: the destination path and blob a write of `report` would put
    /// there, recorded with a write whose outcome is not on record so that
    /// its recovery checks the same blob at the same path even after the
    /// configured paths or templates change. Pure, no request. `None` = the
    /// target records nothing (the default); such a record's recovery
    /// re-renders and uses [`Self::find_published`].
    fn remote_blob(&self, _report: &RenderedReport) -> Option<RemoteBlob> {
        None
    }

    /// R5-5: the receipt (`PublishedArtifact::remote_target`) this target
    /// gives a report written at final `path`, so the stage can find another
    /// record already published there. `None` = the target cannot tell (the
    /// default); no such check is made.
    fn receipt_target(&self, _path: &str) -> Option<String> {
        None
    }

    /// R5-2: read-only lookup of a recorded write: `Some` receipt (at the
    /// commit the lookup read) iff the destination holds `blob.blob_sha` at
    /// `blob.path`; `None` = it does not. Never writes. Only a target whose
    /// [`Self::remote_blob`] records blobs is asked; the default answers
    /// with a retryable error, which leaves the recovery unsettled rather
    /// than reading as "absent".
    async fn find_remote_blob(
        &self,
        _blob: &RemoteBlob,
    ) -> Result<Option<PublishedArtifact>, PublishError> {
        Err(PublishError::api(
            501,
            "this publish target cannot look up a recorded write",
        ))
    }

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
