//! [`DecisionResultRepository`] for both dialects. PostgreSQL calls run in a
//! short transaction with `lock_timeout` / `statement_timeout`, like the
//! `raw_artifacts` purge; transaction-local settings do not leak into the
//! pool.

use async_trait::async_trait;
use sqlx::{PgPool, Postgres, Transaction};
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::{NewRunEvent, RunEventRepository, StorageError, StoragePool, classify_db_error};

use super::decision_result::{
    ClaimedDecision, DecisionArticle, DecisionAttemptRecord, DecisionExecution, DecisionFailure,
    DecisionFailureMode, DecisionIdentity, DecisionRawRecord, DecisionResultRepo,
    DecisionResultRepository, DecisionResultState, DecisionSuccess,
};
use super::decision_result_sql::{
    CLAIM_PG_SQL, CLAIM_SQLITE_SQL, COUNT_OTHER_IDENTITY_PENDING_SQL, GENERATE_PENDING_PG_SQL,
    GENERATE_PENDING_SQLITE_SQL, INSERT_RUN_EVENT_SQL, LOAD_ARTICLE_TEXT_SQL,
    PURGE_EXPIRED_RAW_PG_SQL, PURGE_EXPIRED_RAW_SQLITE_SQL, RECLAIM_EXPIRED_SQL,
    RELEASE_FAILURE_SQL, RELEASE_SUCCESS_SQL, TERMINALIZE_EXHAUSTED_SQL, UPSERT_POLICY_SQL,
};

async fn pg_begin_bounded(pool: &PgPool) -> Result<Transaction<'_, Postgres>, StorageError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '1s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Whole seconds in UTC: the window bound both dialects compare alike.
fn whole_second(value: OffsetDateTime) -> OffsetDateTime {
    let value = value.to_offset(UtcOffset::UTC);
    value - Duration::nanoseconds(i64::from(value.nanosecond()))
}

fn mode_code(mode: DecisionFailureMode) -> i64 {
    match mode {
        DecisionFailureMode::Charge => 0,
        DecisionFailureMode::Permanent => 1,
        DecisionFailureMode::Refund => 2,
    }
}

/// The raw-response columns of one attempt, borrowed for binding.
struct RawColumns<'a> {
    body: Option<&'a str>,
    expires_at: Option<OffsetDateTime>,
    bytes: Option<i64>,
    sha256: Option<&'a str>,
    withheld: Option<&'static str>,
}

fn raw_columns(attempt: &DecisionAttemptRecord) -> RawColumns<'_> {
    match &attempt.raw {
        Some(DecisionRawRecord {
            body,
            expires_at,
            bytes,
            sha256,
            withheld,
        }) => RawColumns {
            body: body.as_deref(),
            // An expiry only means something for a stored body.
            expires_at: body.as_ref().and(*expires_at),
            bytes: Some(*bytes),
            sha256: sha256.as_deref(),
            withheld: *withheld,
        },
        None => RawColumns {
            body: None,
            expires_at: None,
            bytes: None,
            sha256: None,
            withheld: None,
        },
    }
}

macro_rules! bind_success {
    ($query:expr, $success:expr, $id:expr, $owner:expr, $now:expr) => {{
        let success = $success;
        let attempt = &success.attempt;
        let raw = raw_columns(attempt);
        $query
            .bind(&success.answers_json)
            .bind(&success.reported_model)
            .bind(&success.routing_model)
            .bind(&attempt.endpoint_origin)
            .bind(&attempt.state_sha256)
            .bind(attempt.body_chars_sent)
            .bind(attempt.body_truncated)
            .bind(attempt.provider_truncated)
            .bind(attempt.state_tokens_dropped)
            .bind(attempt.input_tokens)
            .bind(attempt.latency_ms)
            .bind(raw.body)
            .bind(raw.expires_at)
            .bind(raw.bytes)
            .bind(raw.sha256)
            .bind(raw.withheld)
            .bind($now)
            .bind($id)
            .bind($owner)
    }};
}

macro_rules! bind_failure {
    ($query:expr, $failure:expr, $id:expr, $owner:expr, $now:expr) => {{
        let failure = $failure;
        let attempt = &failure.attempt;
        let raw = raw_columns(attempt);
        $query
            .bind(mode_code(failure.mode))
            .bind(i64::from(failure.max_attempts))
            .bind($now)
            .bind(&failure.kind)
            .bind(&failure.message)
            .bind(&attempt.endpoint_origin)
            .bind(&attempt.state_sha256)
            .bind(attempt.body_chars_sent)
            .bind(attempt.body_truncated)
            .bind(attempt.latency_ms)
            .bind(raw.body)
            .bind(raw.expires_at)
            .bind(raw.bytes)
            .bind(raw.sha256)
            .bind(raw.withheld)
            .bind($id)
            .bind($owner)
    }};
}

fn parse_state(state: Option<String>) -> Result<Option<DecisionResultState>, StorageError> {
    state
        .map(|state| {
            DecisionResultState::parse(&state)
                .ok_or_else(|| StorageError::Corruption(format!("decision_results.state {state}")))
        })
        .transpose()
}

#[async_trait]
impl DecisionResultRepository for DecisionResultRepo {
    async fn upsert_policy(
        &self,
        policy_version: &str,
        canonical_json: &str,
        now: OffsetDateTime,
    ) -> Result<(), StorageError> {
        macro_rules! query {
            () => {
                sqlx::query(UPSERT_POLICY_SQL)
                    .bind(policy_version)
                    .bind(canonical_json)
                    .bind(now)
            };
        }
        let classify = |error| classify_db_error(error, "decision_policies", policy_version);
        match &self.pool {
            StoragePool::Sqlite(pool) => {
                query!().execute(pool).await.map_err(classify)?;
            }
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                query!().execute(&mut *tx).await.map_err(classify)?;
                tx.commit().await?;
            }
        }
        Ok(())
    }

    async fn generate_pending(
        &self,
        identity: &DecisionIdentity,
        execution: &DecisionExecution,
        cutoff: OffsetDateTime,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        let cutoff = whole_second(cutoff);
        let laya_max_len = execution.laya_max_len.map(i64::from);
        let classify = |error| classify_db_error(error, "decision_results", &identity.category_key);
        let count = match &self.pool {
            StoragePool::Sqlite(pool) => sqlx::query(GENERATE_PENDING_SQLITE_SQL)
                .bind(&identity.category_key)
                .bind(&identity.policy_version)
                .bind(&identity.execution_key)
                .bind(&execution.provider)
                .bind(&execution.requested_model)
                .bind(laya_max_len)
                .bind(now)
                .bind(super::sqlite_utc_seconds(cutoff))
                .bind(i64::from(limit))
                .execute(pool)
                .await
                .map_err(classify)?
                .rows_affected(),
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let count = sqlx::query(GENERATE_PENDING_PG_SQL)
                    .bind(&identity.category_key)
                    .bind(&identity.policy_version)
                    .bind(&identity.execution_key)
                    .bind(&execution.provider)
                    .bind(&execution.requested_model)
                    .bind(laya_max_len)
                    .bind(now)
                    .bind(cutoff)
                    .bind(i64::from(limit))
                    .execute(&mut *tx)
                    .await
                    .map_err(classify)?
                    .rows_affected();
                tx.commit().await?;
                count
            }
        };
        Ok(count)
    }

    async fn claim(
        &self,
        identity: &DecisionIdentity,
        owner: &str,
        lease_expires_at: OffsetDateTime,
        max_attempts: u32,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedDecision>, StorageError> {
        let query = |sql| {
            sqlx::query_as::<_, ClaimedDecision>(sql)
                .bind(owner)
                .bind(lease_expires_at)
                .bind(now)
                .bind(&identity.category_key)
                .bind(&identity.policy_version)
                .bind(&identity.execution_key)
                .bind(i64::from(max_attempts))
                .bind(i64::from(limit))
        };
        match &self.pool {
            StoragePool::Sqlite(pool) => Ok(query(CLAIM_SQLITE_SQL).fetch_all(pool).await?),
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let claimed = sqlx::query_as::<_, ClaimedDecision>(CLAIM_PG_SQL)
                    .bind(owner)
                    .bind(lease_expires_at)
                    .bind(now)
                    .bind(&identity.category_key)
                    .bind(&identity.policy_version)
                    .bind(&identity.execution_key)
                    .bind(i64::from(max_attempts))
                    .bind(i64::from(limit))
                    .fetch_all(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(claimed)
            }
        }
    }

    async fn release_success(
        &self,
        id: i64,
        owner: &str,
        success: &DecisionSuccess,
        now: OffsetDateTime,
    ) -> Result<bool, StorageError> {
        let affected = match &self.pool {
            StoragePool::Sqlite(pool) => {
                bind_success!(sqlx::query(RELEASE_SUCCESS_SQL), success, id, owner, now)
                    .execute(pool)
                    .await?
                    .rows_affected()
            }
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let affected =
                    bind_success!(sqlx::query(RELEASE_SUCCESS_SQL), success, id, owner, now)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                tx.commit().await?;
                affected
            }
        };
        Ok(affected == 1)
    }

    async fn release_failure(
        &self,
        id: i64,
        owner: &str,
        failure: &DecisionFailure,
        now: OffsetDateTime,
    ) -> Result<Option<DecisionResultState>, StorageError> {
        let state = match &self.pool {
            StoragePool::Sqlite(pool) => {
                bind_failure!(
                    sqlx::query_scalar::<_, String>(RELEASE_FAILURE_SQL),
                    failure,
                    id,
                    owner,
                    now
                )
                .fetch_optional(pool)
                .await?
            }
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let state = bind_failure!(
                    sqlx::query_scalar::<_, String>(RELEASE_FAILURE_SQL),
                    failure,
                    id,
                    owner,
                    now
                )
                .fetch_optional(&mut *tx)
                .await?;
                tx.commit().await?;
                state
            }
        };
        parse_state(state)
    }

    async fn reclaim_expired(
        &self,
        identity: &DecisionIdentity,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        macro_rules! query {
            () => {
                sqlx::query(RECLAIM_EXPIRED_SQL)
                    .bind(now)
                    .bind(&identity.category_key)
                    .bind(&identity.policy_version)
                    .bind(&identity.execution_key)
            };
        }
        match &self.pool {
            StoragePool::Sqlite(pool) => Ok(query!().execute(pool).await?.rows_affected()),
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let count = query!().execute(&mut *tx).await?.rows_affected();
                tx.commit().await?;
                Ok(count)
            }
        }
    }

    async fn terminalize_exhausted(
        &self,
        identity: &DecisionIdentity,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<u64, StorageError> {
        macro_rules! query {
            () => {
                sqlx::query(TERMINALIZE_EXHAUSTED_SQL)
                    .bind(now)
                    .bind(&identity.category_key)
                    .bind(&identity.policy_version)
                    .bind(&identity.execution_key)
                    .bind(i64::from(max_attempts))
            };
        }
        match &self.pool {
            StoragePool::Sqlite(pool) => Ok(query!().execute(pool).await?.rows_affected()),
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let count = query!().execute(&mut *tx).await?.rows_affected();
                tx.commit().await?;
                Ok(count)
            }
        }
    }

    async fn purge_expired_raw(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<u64, StorageError> {
        match &self.pool {
            StoragePool::Sqlite(pool) => Ok(sqlx::query(PURGE_EXPIRED_RAW_SQLITE_SQL)
                .bind(now)
                .bind(i64::from(limit))
                .execute(pool)
                .await?
                .rows_affected()),
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let count = sqlx::query(PURGE_EXPIRED_RAW_PG_SQL)
                    .bind(now)
                    .bind(i64::from(limit))
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                tx.commit().await?;
                Ok(count)
            }
        }
    }

    async fn load_article(&self, article_id: i64) -> Result<Option<DecisionArticle>, StorageError> {
        macro_rules! query {
            () => {
                sqlx::query_as::<_, DecisionArticle>(LOAD_ARTICLE_TEXT_SQL).bind(article_id)
            };
        }
        match &self.pool {
            StoragePool::Sqlite(pool) => Ok(query!().fetch_optional(pool).await?),
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let article = query!().fetch_optional(&mut *tx).await?;
                tx.commit().await?;
                Ok(article)
            }
        }
    }

    async fn count_other_identity_pending(
        &self,
        identity: &DecisionIdentity,
    ) -> Result<u64, StorageError> {
        macro_rules! query {
            () => {
                sqlx::query_scalar::<_, i64>(COUNT_OTHER_IDENTITY_PENDING_SQL)
                    .bind(&identity.category_key)
                    .bind(&identity.policy_version)
                    .bind(&identity.execution_key)
            };
        }
        let count = match &self.pool {
            StoragePool::Sqlite(pool) => query!().fetch_one(pool).await?,
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let count = query!().fetch_one(&mut *tx).await?;
                tx.commit().await?;
                count
            }
        };
        u64::try_from(count).map_err(|_| StorageError::Corruption("negative COUNT(*)".to_string()))
    }
}

#[async_trait]
impl RunEventRepository for DecisionResultRepo {
    async fn insert(&self, event: &NewRunEvent) -> Result<i64, StorageError> {
        macro_rules! query {
            () => {
                sqlx::query_scalar::<_, i64>(INSERT_RUN_EVENT_SQL)
                    .bind(&event.run_id)
                    .bind(&event.trace_id)
                    .bind(&event.stage)
                    .bind(&event.severity)
                    .bind(&event.event_kind)
                    .bind(&event.target_kind)
                    .bind(event.target_id)
                    .bind(&event.message)
                    .bind(&event.context_json)
            };
        }
        let classify = |error| classify_db_error(error, "run_events", &event.run_id);
        match &self.pool {
            StoragePool::Sqlite(pool) => query!().fetch_one(pool).await.map_err(classify),
            StoragePool::Postgres(pool) => {
                let mut tx = pg_begin_bounded(pool).await?;
                let id = query!().fetch_one(&mut *tx).await.map_err(classify)?;
                tx.commit().await?;
                Ok(id)
            }
        }
    }
}
