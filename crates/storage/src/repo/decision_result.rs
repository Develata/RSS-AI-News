//! decision_policies / decision_results persistence (ADR 0011 §2–§8).
//!
//! The typed-decision shadow runner owns these two tables and this state
//! machine; nothing else reads them, so the feature can be removed with one
//! drop migration. The state machine lives here rather than in
//! `domain::state` on purpose (ADR 0011 §2).
//!
//! Every task statement is scoped to one identity
//! (`category_key`, `policy_version`, `execution_key`): a run never executes a
//! row generated for another policy or provider. On PostgreSQL each call runs
//! in a short transaction with server-side `lock_timeout` / `statement_timeout`;
//! the caller adds the client-side deadline.

use async_trait::async_trait;
use sqlx::FromRow;
use time::OffsetDateTime;

use crate::{StorageError, StoragePool};

/// `pending → running → succeeded | failed`; `running → pending` on a
/// retryable failure, a refund or lease reclaim; `pending → failed` when the
/// attempt budget is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionResultState {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl DecisionResultState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// The identity a run works on (ADR 0011 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionIdentity {
    pub category_key: String,
    pub policy_version: String,
    pub execution_key: String,
}

/// Execution parameters written with each generated row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionExecution {
    pub provider: String,
    pub requested_model: String,
    /// `Some` for Laya only (enforced by a CHECK).
    pub laya_max_len: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct ClaimedDecision {
    pub id: i64,
    pub article_id: i64,
    pub policy_version: String,
    pub execution_key: String,
    pub attempt_count: i64,
}

/// A stored 2xx body and its fingerprint (ADR 0011 §8).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct DecisionRawRecord {
    /// The body itself, only when it may be kept.
    pub body: Option<String>,
    /// Required when `body` is `Some` and a TTL applies.
    pub expires_at: Option<OffsetDateTime>,
    pub bytes: i64,
    pub sha256: Option<String>,
    /// `not_json` / `credential` / `incomplete`.
    pub withheld: Option<&'static str>,
}

/// Lengths and digest only.
impl std::fmt::Debug for DecisionRawRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionRawRecord")
            .field("stored", &self.body.is_some())
            .field("expires_at", &self.expires_at)
            .field("bytes", &self.bytes)
            .field("sha256", &self.sha256)
            .field("withheld", &self.withheld)
            .finish()
    }
}

/// What one attempt recorded, success or not. `None` means unknown (e.g.
/// the request was never built).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionAttemptRecord {
    pub endpoint_origin: Option<String>,
    pub state_sha256: Option<String>,
    pub body_chars_sent: Option<i64>,
    pub body_truncated: Option<bool>,
    pub provider_truncated: Option<bool>,
    pub state_tokens_dropped: Option<i64>,
    pub input_tokens: Option<i64>,
    pub latency_ms: Option<i64>,
    pub raw: Option<DecisionRawRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionSuccess {
    pub answers_json: String,
    pub reported_model: String,
    pub routing_model: Option<String>,
    pub attempt: DecisionAttemptRecord,
}

/// How a failed attempt is released (ADR 0011 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionFailureMode {
    /// Back to `pending`, the attempt counts; `failed` once it was the last.
    Charge,
    /// Straight to `failed`.
    Permanent,
    /// Back to `pending` and the attempt is refunded.
    Refund,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionFailure {
    pub mode: DecisionFailureMode,
    pub kind: String,
    /// Fixed text only; never provider content.
    pub message: String,
    pub max_attempts: u32,
    pub attempt: DecisionAttemptRecord,
}

#[async_trait]
pub trait DecisionResultRepository: Send + Sync {
    /// Stores a policy snapshot once; an existing row is left as is.
    async fn upsert_policy(
        &self,
        policy_version: &str,
        canonical_json: &str,
        now: OffsetDateTime,
    ) -> Result<(), StorageError>;

    /// Inserts up to `limit` `pending` rows for articles of the category
    /// created at or after `cutoff` (whole seconds), not retired and without a
    /// row for this identity. Returns the number inserted.
    async fn generate_pending(
        &self,
        identity: &DecisionIdentity,
        execution: &DecisionExecution,
        cutoff: OffsetDateTime,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError>;

    /// Claims up to `limit` claimable `pending` rows of this identity.
    #[allow(clippy::too_many_arguments)]
    async fn claim(
        &self,
        identity: &DecisionIdentity,
        owner: &str,
        lease_expires_at: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedDecision>, StorageError>;

    /// `false`: the owner guard missed; nothing was written.
    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        success: &DecisionSuccess,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError>;

    /// The resulting state; `None` when the owner guard missed.
    async fn release_failure(
        &self,
        id: i64,
        owner: &str,
        failure: &DecisionFailure,
        now: OffsetDateTime,
    ) -> Result<Option<DecisionResultState>, StorageError>;

    /// Expired `running` rows of this identity back to `pending`.
    async fn reclaim_expired(
        &self,
        identity: &DecisionIdentity,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError>;

    /// `pending` rows of this identity with the attempt budget spent and no
    /// live lease → `failed` (`retry_budget_exhausted`).
    async fn terminalize_exhausted(
        &self,
        identity: &DecisionIdentity,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError>;

    /// Clears up to `limit` expired raw bodies across every identity.
    async fn purge_expired_raw(&self, now: OffsetDateTime, limit: u32)
    -> Result<u64, StorageError>;

    /// `pending` rows of the category that belong to another identity.
    async fn count_other_identity_pending(
        &self,
        identity: &DecisionIdentity,
    ) -> Result<u64, StorageError>;
}

#[derive(Debug, Clone)]
pub struct DecisionResultRepo {
    pub(super) pool: StoragePool,
}

impl DecisionResultRepo {
    pub fn new_with_storage(pool: StoragePool) -> Self {
        Self { pool }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_through_their_column_text() {
        for state in [
            DecisionResultState::Pending,
            DecisionResultState::Running,
            DecisionResultState::Succeeded,
            DecisionResultState::Failed,
        ] {
            assert_eq!(DecisionResultState::parse(state.as_str()), Some(state));
        }
        assert_eq!(DecisionResultState::parse("filtered"), None);
    }

    #[test]
    fn raw_record_debug_never_prints_the_body() {
        let record = DecisionRawRecord {
            body: Some("{\"secret\":1}".to_string()),
            ..DecisionRawRecord::default()
        };
        assert!(!format!("{record:?}").contains("secret"));
    }
}
