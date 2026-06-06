//! publish flow 的 Options / Outcome / Status DTO。
//!
//! 由 [`super`] 经 `pub use dto::*` 重导出，对外 API 路径不变。

use std::num::NonZeroU32;

use rss_ai_news_domain::Score0To100;
use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub struct PublishInitOptions {
    pub category_key: String,
    pub report_date: String,
    pub target_timezone: String,
    pub render_version: i64,
    pub selection_policy_version: i64,
    pub remote_target: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublishFreezeOptions {
    pub category_key: String,
    pub max_items: NonZeroU32,
    pub min_importance_score: Score0To100,
    pub include_unscored: bool,
    pub ai_enabled: bool,
    pub candidate_window_hours: u32,
    pub excerpt_max_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishInitOutcome {
    Created {
        publish_record_id: i64,
    },
    AlreadyExists {
        publish_record_id: i64,
        state: String,
    },
}

#[derive(Debug, Clone)]
pub struct PublishFreezeOutcome {
    pub publish_record_id: i64,
    pub status: PublishFreezeStatus,
    pub item_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishFreezeStatus {
    Frozen,
    SnapshotEmpty,
    NothingToClaim,
    Conflicted,
    ArticleConflict { article_id: i64 },
    Failed { error_kind: String },
}

#[derive(Debug, Clone)]
pub struct PublishRenderOptions {
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at: OffsetDateTime,
    pub path_template: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublishStoreLocalOptions {
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at: OffsetDateTime,
    pub path_template: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublishRemoteOptions {
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at: OffsetDateTime,
    pub path_template: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublishRemoteBatchItemOptions {
    pub publish_record_id: i64,
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at: OffsetDateTime,
    pub path_template: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublishRemoteBatchOptions {
    pub items: Vec<PublishRemoteBatchItemOptions>,
}

#[derive(Debug, Clone)]
pub struct PublishRenderOutcome {
    pub publish_record_id: i64,
    pub status: PublishRenderStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishRenderStatus {
    Rendered,
    NothingToClaim,
    Conflicted,
    Failed { error_kind: String },
}

#[derive(Debug, Clone)]
pub struct PublishStoreLocalOutcome {
    pub publish_record_id: i64,
    pub status: PublishStoreLocalStatus,
    pub local_path: Option<String>,
    pub item_count: u32,
}

#[derive(Debug, Clone)]
pub struct PublishRemoteOutcome {
    pub publish_record_id: i64,
    pub status: PublishRemoteStatus,
    pub commit_sha: Option<String>,
    pub remote_target: Option<String>,
    pub item_count: u32,
}

#[derive(Debug, Clone)]
pub struct PublishRemoteBatchOutcome {
    pub items: Vec<PublishRemoteOutcome>,
    pub commit_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishStoreLocalStatus {
    StoredLocal,
    PublishedLocal,
    NothingToClaim,
    Conflicted,
    ArticleConflict { article_id: i64 },
    Failed { error_kind: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishRemoteStatus {
    PublishedRemote,
    NothingToClaim,
    Conflicted,
    ArticleConflict { article_id: i64 },
    MissingTarget,
    Failed { error_kind: String },
}
