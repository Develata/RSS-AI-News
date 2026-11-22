//! Local + remote (GitHub) publishing of rendered reports.

pub mod error;
pub mod github;
pub mod local;
pub mod target;

pub use error::{GitHubRequestContext, PublishError, REMOTE_WRITE_UNVERIFIED_KIND};
pub use github::{GitHubTarget, GitHubTargetConfig};
pub use local::LocalFsTarget;
pub use target::{PublishTarget, PublishedArtifact, PublishedBatchArtifact};
