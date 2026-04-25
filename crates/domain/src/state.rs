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

    fn assert_round_trip<T>(value: T, serialized: &str)
    where
        T: fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(&value).expect("state serialization should succeed");
        assert_eq!(json, format!("\"{serialized}\""));

        let decoded: T = serde_json::from_str(&json).expect("state deserialization should succeed");
        assert_eq!(decoded, value);
    }

    #[test]
    fn state_enums_use_snake_case_round_trip_values() {
        assert_round_trip(FeedEntryState::PendingFetch, "pending_fetch");
        assert_round_trip(ArticleState::ReadyForPublish, "ready_for_publish");
        assert_round_trip(AiResultState::Pending, "pending");
        assert_round_trip(AiResultState::PermanentFailed, "permanent_failed");
        assert_round_trip(PublishState::PublishedLocal, "published_local");
        assert_round_trip(PublishState::PublishedRemote, "published_remote");
        assert_round_trip(FeedSourceStatus::Archived, "archived");
        assert_round_trip(FeedKind::JsonFeed, "json_feed");
        assert_round_trip(DedupDecision::UidDup, "uid_dup");
        assert_round_trip(ExtractorStrategy::SummaryFallback, "summary_fallback");
        assert_round_trip(ContentQuality::Fallback, "fallback");
        assert_round_trip(ArtifactKind::AiRawResponse, "ai_raw_response");
        assert_round_trip(BackfillTarget::Ai, "ai");
    }

    #[test]
    fn ai_result_state_rejects_removed_retryable_failed_variant() {
        let result = serde_json::from_str::<AiResultState>("\"retryable_failed\"");
        assert!(result.is_err());
    }
}
