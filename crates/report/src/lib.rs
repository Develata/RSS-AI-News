//! Publish candidate selection + snapshot freezing + Markdown rendering.

pub mod error;
pub mod excerpt;
pub mod frontmatter;
pub mod render;
pub mod selection;
pub mod snapshot;

pub use error::ReportError;
pub use excerpt::generate_excerpt;
pub use frontmatter::build_frontmatter;
pub use render::{RenderConfig, render_markdown};
pub use selection::{SelectionConfig, load_candidates};
pub use snapshot::{SnapshotConfig, freeze, to_storage_items};
