//! Local + remote (GitHub) publishing of rendered reports.

pub mod error;
pub mod local;
pub mod target;

pub use error::PublishError;
pub use local::LocalFsTarget;
pub use target::{PublishTarget, PublishedArtifact};
