//! State enums for all state machines.
//!
//! These are the single source of truth for valid states.
//! See docs/design/state-machine.md for transition rules.

use serde::{Deserialize, Serialize};
use std::fmt;

/// FeedEntry processing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum FeedEntryState {
    Discovered,
    DedupSkipped,
    PendingFetch,
    Fetching,
    Extracting,
    Persisted,
    FallbackPersisted,
    Failed,
}

/// Article lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum ArticleState {
    Persisted,
    AiPending,
    AiDone,
    ReadyForPublish,
    PublishSkipped,
    Published,
    Retired,
}

/// AI result processing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum AiResultState {
    Pending,
    Running,
    Succeeded,
    PermanentFailed,
    Filtered,
}

/// Publish record state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum PublishState {
    Pending,
    SnapshotFrozen,
    Rendered,
    StoredLocal,
    PublishedLocal,
    PublishedRemote,
    Failed,
}

/// Feed source status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum FeedSourceStatus {
    Active,
    Paused,
    Archived,
}

/// Feed format kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum FeedKind {
    Rss,
    Atom,
    JsonFeed,
    RssHub,
}

/// Dedup decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum DedupDecision {
    Fresh,
    UidDup,
    LinkDup,
    HashDup,
}

/// Extractor strategy used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum ExtractorStrategy {
    Readability,
    Rule,
    SummaryFallback,
}

/// Content quality assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum ContentQuality {
    High,
    Medium,
    Fallback,
}

/// Raw artifact kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum ArtifactKind {
    FeedPayload,
    HtmlPayload,
    AiRawResponse,
}

/// Backfill target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum BackfillTarget {
    Extract,
    Ai,
}

/// Rule version lifecycle status. See `docs/design/storage-schema.md` §4.8 +
/// `state-machine.md` §6 (reindex_job state machine).
///
/// - `Active`: currently effective; at most one row per `kind` may be `Active`.
/// - `Pending`: created by reindex but not yet activated; resolvers using
///   `active_rule(kind)` ignore these rows.
/// - `Superseded`: replaced by a newer `Active` row; preserved for audit
///   joins. `retired_at` is non-NULL for these rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum RuleVersionStatus {
    Pending,
    Active,
    Superseded,
}

/// Reindex job state machine. See `docs/design/state-machine.md` §6.2
/// (reindex_job state set) + `storage-schema.md` §4.10 (`reindex_jobs.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum ReindexJobState {
    Pending,
    Running,
    Completed,
    Failed,
    Aborted,
}

/// Reindex target — what the job rewrites. See `state-machine.md` §6.1 +
/// `storage-schema.md` §4.10 (`reindex_jobs.target`) + `cli-semantics.md`
/// §4.8 (`reindex --target`).
///
/// String form is **snake_case** (`link_hash` / `content_hash` /
/// `categories`) per `internal-dto-contracts.md` §8 — uniform with all other
/// domain enums across serde, sqlx, run_events, and CLI input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum ReindexTarget {
    LinkHash,
    ContentHash,
    Categories,
}

impl fmt::Display for ReindexTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

impl fmt::Display for FeedEntryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_all_round_trip<T>(cases: &[(T, &str)])
    where
        T: Copy + fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>,
    {
        for (value, expected) in cases {
            let json = serde_json::to_string(value).expect("state serialization should succeed");
            assert_eq!(json, format!("\"{expected}\""), "serialize {value:?}");

            let decoded: T =
                serde_json::from_str(&json).expect("state deserialization should succeed");
            assert_eq!(decoded, *value, "deserialize {expected}");
        }
    }

    #[test]
    fn feed_entry_state_round_trip_all_variants() {
        assert_all_round_trip(&[
            (FeedEntryState::Discovered, "discovered"),
            (FeedEntryState::DedupSkipped, "dedup_skipped"),
            (FeedEntryState::PendingFetch, "pending_fetch"),
            (FeedEntryState::Fetching, "fetching"),
            (FeedEntryState::Extracting, "extracting"),
            (FeedEntryState::Persisted, "persisted"),
            (FeedEntryState::FallbackPersisted, "fallback_persisted"),
            (FeedEntryState::Failed, "failed"),
        ]);
    }

    #[test]
    fn article_state_round_trip_all_variants() {
        assert_all_round_trip(&[
            (ArticleState::Persisted, "persisted"),
            (ArticleState::AiPending, "ai_pending"),
            (ArticleState::AiDone, "ai_done"),
            (ArticleState::ReadyForPublish, "ready_for_publish"),
            (ArticleState::PublishSkipped, "publish_skipped"),
            (ArticleState::Published, "published"),
            (ArticleState::Retired, "retired"),
        ]);
    }

    #[test]
    fn ai_result_state_round_trip_all_variants() {
        assert_all_round_trip(&[
            (AiResultState::Pending, "pending"),
            (AiResultState::Running, "running"),
            (AiResultState::Succeeded, "succeeded"),
            (AiResultState::PermanentFailed, "permanent_failed"),
            (AiResultState::Filtered, "filtered"),
        ]);
    }

    #[test]
    fn publish_state_round_trip_all_variants() {
        assert_all_round_trip(&[
            (PublishState::Pending, "pending"),
            (PublishState::SnapshotFrozen, "snapshot_frozen"),
            (PublishState::Rendered, "rendered"),
            (PublishState::StoredLocal, "stored_local"),
            (PublishState::PublishedLocal, "published_local"),
            (PublishState::PublishedRemote, "published_remote"),
            (PublishState::Failed, "failed"),
        ]);
    }

    #[test]
    fn feed_source_status_round_trip_all_variants() {
        assert_all_round_trip(&[
            (FeedSourceStatus::Active, "active"),
            (FeedSourceStatus::Paused, "paused"),
            (FeedSourceStatus::Archived, "archived"),
        ]);
    }

    #[test]
    fn feed_kind_round_trip_all_variants() {
        assert_all_round_trip(&[
            (FeedKind::Rss, "rss"),
            (FeedKind::Atom, "atom"),
            (FeedKind::JsonFeed, "json_feed"),
            (FeedKind::RssHub, "rss_hub"),
        ]);
    }

    #[test]
    fn dedup_decision_round_trip_all_variants() {
        assert_all_round_trip(&[
            (DedupDecision::Fresh, "fresh"),
            (DedupDecision::UidDup, "uid_dup"),
            (DedupDecision::LinkDup, "link_dup"),
            (DedupDecision::HashDup, "hash_dup"),
        ]);
    }

    #[test]
    fn extractor_strategy_round_trip_all_variants() {
        assert_all_round_trip(&[
            (ExtractorStrategy::Readability, "readability"),
            (ExtractorStrategy::Rule, "rule"),
            (ExtractorStrategy::SummaryFallback, "summary_fallback"),
        ]);
    }

    #[test]
    fn content_quality_round_trip_all_variants() {
        assert_all_round_trip(&[
            (ContentQuality::High, "high"),
            (ContentQuality::Medium, "medium"),
            (ContentQuality::Fallback, "fallback"),
        ]);
    }

    #[test]
    fn artifact_kind_round_trip_all_variants() {
        assert_all_round_trip(&[
            (ArtifactKind::FeedPayload, "feed_payload"),
            (ArtifactKind::HtmlPayload, "html_payload"),
            (ArtifactKind::AiRawResponse, "ai_raw_response"),
        ]);
    }

    #[test]
    fn backfill_target_round_trip_all_variants() {
        assert_all_round_trip(&[
            (BackfillTarget::Extract, "extract"),
            (BackfillTarget::Ai, "ai"),
        ]);
    }

    #[test]
    fn rule_version_status_round_trip_all_variants() {
        assert_all_round_trip(&[
            (RuleVersionStatus::Pending, "pending"),
            (RuleVersionStatus::Active, "active"),
            (RuleVersionStatus::Superseded, "superseded"),
        ]);
    }

    #[test]
    fn reindex_job_state_round_trip_all_variants() {
        assert_all_round_trip(&[
            (ReindexJobState::Pending, "pending"),
            (ReindexJobState::Running, "running"),
            (ReindexJobState::Completed, "completed"),
            (ReindexJobState::Failed, "failed"),
            (ReindexJobState::Aborted, "aborted"),
        ]);
    }

    #[test]
    fn reindex_target_round_trip_all_variants() {
        assert_all_round_trip(&[
            (ReindexTarget::LinkHash, "link_hash"),
            (ReindexTarget::ContentHash, "content_hash"),
            (ReindexTarget::Categories, "categories"),
        ]);
    }

    #[test]
    fn reindex_target_display_matches_snake_case() {
        assert_eq!(ReindexTarget::LinkHash.to_string(), "link_hash");
        assert_eq!(ReindexTarget::ContentHash.to_string(), "content_hash");
        assert_eq!(ReindexTarget::Categories.to_string(), "categories");
    }

    #[test]
    fn ai_result_state_rejects_removed_retryable_failed_variant() {
        let result = serde_json::from_str::<AiResultState>("\"retryable_failed\"");
        assert!(result.is_err());
    }
}
