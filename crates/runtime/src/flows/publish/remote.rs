//! [`PublishFlow`] 的 remote 阶段：单条 publish_remote + 批量 publish_remote_batch。
//!
//! `PreparedRemote` 为本阶段私有的批量预备态。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use rss_ai_news_domain::dto::publish::{RemoteBlob, RenderedReport};
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_publish::{
    PublishError, PublishTarget, PublishedArtifact, REMOTE_WRITE_UNVERIFIED_KIND,
};
use rss_ai_news_report::{RenderConfig, ReportError, load_frozen_items, render_markdown};
use rss_ai_news_storage::{
    ClaimRequest, ClaimedPublishRecord, PublishAdvanceExtras, PublishItemRepository, PublishState,
    PublishTimestampField, TerminalAdvanceOutcome, TerminalAdvanceStatus, UnverifiedWriteUpdate,
    build_owner_id, lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::time::Instant;

use super::dto::{
    PublishRemoteBatchOptions, PublishRemoteBatchOutcome, PublishRemoteOptions,
    PublishRemoteOutcome, PublishRemoteStatus,
};
use super::{OwnershipGuard, PublishFlow};
use super::{render_templates_from_ctx, rendered_timestamp};
use crate::events::RunEventEmitter;

struct PreparedRemote {
    publish_record_id: i64,
    promote_article_ids: Vec<i64>,
    item_count: u32,
    /// R5-2: what the batch write puts at the destination for this record.
    blob: Option<RemoteBlob>,
    report: RenderedReport,
}

// Start the shared preparation/remote budget before claiming so claim latency
// consumes it too. Reserve 20% for releases; claim and cleanup database writes
// retain their own failure/recovery semantics rather than being cancelled here.
fn remote_deadline(lease_seconds: u64) -> Instant {
    let lease = StdDuration::from_secs(lease_seconds);
    Instant::now() + (lease - lease / 5)
}

/// P-B2: a remote call cut off by [`remote_deadline`] may already have
/// written. Half of the reserved 20% of the lease is given to a read-only
/// HEAD check; the other half stays for the database release. The
/// owner-guarded advance still refuses a record whose lease was lost.
fn verify_deadline(deadline: Instant, lease_seconds: u64) -> Instant {
    deadline + StdDuration::from_secs(lease_seconds) / 10
}

/// Read-only receipt lookup after a remote timeout. `Ok` = the destination
/// holds every report; `RemoteTimeout` = it does not (the timeout stands);
/// `RemoteWriteUnverified` = the check failed or ran out of time, so whether
/// the cut-off call wrote is unknown (R3F-B2).
async fn verify_after_timeout(
    target: &dyn PublishTarget,
    reports: &[RenderedReport],
    until: Instant,
) -> Result<rss_ai_news_publish::PublishedBatchArtifact, PublishError> {
    let unverified = |verification: PublishError| PublishError::RemoteWriteUnverified {
        write: Box::new(PublishError::RemoteTimeout),
        verification: Box::new(verification),
    };
    if Instant::now() >= until {
        return Err(unverified(PublishError::RemoteTimeout));
    }
    match tokio::time::timeout_at(until, target.find_published(reports)).await {
        Ok(Ok(Some(batch))) if batch.artifacts.len() == reports.len() => {
            tracing::info!(
                reports = reports.len(),
                commit_sha = batch.commit_sha.as_deref(),
                "remote publish timed out but the destination already holds every report"
            );
            Ok(batch)
        }
        Ok(Ok(_)) => Err(PublishError::RemoteTimeout),
        Ok(Err(error)) => {
            tracing::warn!(?error, "post-timeout HEAD check failed; outcome unverified");
            Err(unverified(error))
        }
        Err(_) => {
            tracing::warn!("post-timeout HEAD check ran out of time; outcome unverified");
            Err(unverified(PublishError::RemoteTimeout))
        }
    }
}

/// `last_error` reason of a recovery deferred before its check settled (R4-1).
pub(super) const EARLIER_WRITE_UNVERIFIED: &str =
    "github write of an earlier attempt still unverified";
/// `last_error` reason of a receipt the destination holds but the database
/// did not record (R5-1).
const RECEIPT_NOT_RECORDED: &str = "github holds the report but its receipt was not recorded";

/// R4-1: the record's previous remote attempt ended
/// `remote_write_unverified` — its write may have landed.
fn has_unverified_write(claimed: &ClaimedPublishRecord) -> bool {
    claimed.last_error_kind.as_deref() == Some(REMOTE_WRITE_UNVERIFIED_KIND)
}

/// R4-1: read-only fixed-HEAD content check for a record whose earlier write
/// is unverified, run before any write. `Ok(Some((receipt, blob)))` = the
/// destination already holds it (record this receipt — for `blob` — and send
/// nothing); `Ok(None)` = it is confirmed absent (write normally, charged
/// normally); `Err` = the check failed, was rate limited or ran out of the
/// remote deadline, so nothing is settled:
/// [`PublishError::EarlierWriteUnverified`], released deferred.
///
/// R5-2: with a `recorded` write the check looks for exactly that blob at
/// exactly that path — where the earlier write went, whatever the configured
/// prefix, path template or report templates say now. Without one (a target
/// that records nothing, or a record marked before migration 0010) it looks
/// for the report as rendered now.
async fn check_earlier_write(
    target: &dyn PublishTarget,
    report: &RenderedReport,
    recorded: Option<&RemoteBlob>,
    deadline: Instant,
) -> Result<Option<(PublishedArtifact, Option<RemoteBlob>)>, PublishError> {
    let unsettled = |verification: PublishError| PublishError::EarlierWriteUnverified {
        verification: Box::new(verification),
    };
    if Instant::now() >= deadline {
        return Err(unsettled(PublishError::RemoteTimeout));
    }
    if let Some(recorded) = recorded {
        return match tokio::time::timeout_at(deadline, target.find_remote_blob(recorded)).await {
            Ok(Ok(Some(artifact))) => {
                tracing::info!(
                    commit_sha = artifact.commit_sha.as_deref(),
                    path = recorded.path.as_str(),
                    "earlier unverified write found at HEAD; recording its receipt"
                );
                Ok(Some((artifact, Some(recorded.clone()))))
            }
            Ok(Ok(None)) => Ok(None),
            Ok(Err(error)) => Err(unsettled(error)),
            Err(_) => Err(unsettled(PublishError::RemoteTimeout)),
        };
    }
    match tokio::time::timeout_at(
        deadline,
        target.find_published(std::slice::from_ref(report)),
    )
    .await
    {
        Ok(Ok(Some(mut batch))) if batch.artifacts.len() == 1 => {
            tracing::info!(
                commit_sha = batch.commit_sha.as_deref(),
                "earlier unverified write found at HEAD; recording its receipt"
            );
            Ok(Some((
                batch.artifacts.remove(0),
                target.remote_blob(report),
            )))
        }
        // A target breaking the one-artifact-per-report contract settles
        // nothing either.
        Ok(Ok(Some(batch))) => Err(unsettled(PublishError::api(
            502,
            format!(
                "find_published returned {} artifacts for 1 report",
                batch.artifacts.len()
            ),
        ))),
        Ok(Ok(None)) => Ok(None),
        Ok(Err(error)) => Err(unsettled(error)),
        Err(_) => Err(unsettled(PublishError::RemoteTimeout)),
    }
}

/// R5-5: kind of a write held because another category's record holds its
/// destination.
pub(super) const REMOTE_PATH_CONFLICT_KIND: &str = "remote_path_conflict";
/// Outcome kind of every item of a batch that repeats a record id.
const DUPLICATE_BATCH_RECORD_KIND: &str = "duplicate_publish_record";
/// R5-3: `last_error` while a write is in flight (recorded before sending).
const PENDING_WRITE_NOTE: &str = "github write sent; its outcome is not recorded yet";

/// R5-4: the recovery check confirmed the recorded write absent at its
/// recorded path. If this attempt would write elsewhere, a request of the
/// earlier attempt may still land there and leave two copies, so the attempt
/// is held: `Some` = [`PublishError::EarlierWriteUnverified`] carrying
/// [`PublishError::RemoteDestinationChanged`] (deferred, recorded write
/// kept). Same path → `None`, write (GitHub's update-by-sha and the batch's
/// fast-forward-only ref update keep one file there). No recorded write (rows
/// marked before migration 0010, targets that record none) → `None`, as
/// R4-1.
fn destination_changed(
    recorded: Option<&RemoteBlob>,
    written: Option<&RemoteBlob>,
) -> Option<PublishError> {
    let recorded = recorded?;
    if written.is_some_and(|written| written.path == recorded.path) {
        return None;
    }
    Some(PublishError::EarlierWriteUnverified {
        verification: Box::new(PublishError::RemoteDestinationChanged {
            recorded: recorded.path.clone(),
            current: written.map(|written| written.path.clone()),
        }),
    })
}

/// R5-2: what a deferred release of `error` does with the recorded write.
/// `written` = the blob this attempt's write would put at the destination.
/// - [`PublishError::RemoteWriteUnverified`]: this attempt's write may have
///   landed — record it (replacing an earlier one, which the recovery check
///   confirmed absent before this write was sent);
/// - [`PublishError::EarlierWriteUnverified`]: nothing settled — keep the
///   write the record inherited;
/// - anything else deferred (rate limits): no write is unsettled — clear.
fn unverified_write_update<'a>(
    error: &PublishError,
    written: Option<&'a RemoteBlob>,
) -> UnverifiedWriteUpdate<'a> {
    match error {
        PublishError::RemoteWriteUnverified { .. } => UnverifiedWriteUpdate::Record(written),
        PublishError::EarlierWriteUnverified { .. } => UnverifiedWriteUpdate::Keep,
        _ => UnverifiedWriteUpdate::Record(None),
    }
}

/// The single-report write within `deadline`; a call cut off by it is
/// verified read-only before it is reported (P-B2).
async fn send_report(
    target: &dyn PublishTarget,
    report: &RenderedReport,
    deadline: Instant,
    lease_seconds: u64,
) -> Result<PublishedArtifact, PublishError> {
    if Instant::now() >= deadline {
        return Err(PublishError::RemoteTimeout);
    }
    match tokio::time::timeout_at(deadline, target.publish(report)).await {
        Ok(result) => result,
        // The cancelled call may already have written.
        Err(_) => verify_after_timeout(
            target,
            std::slice::from_ref(report),
            verify_deadline(deadline, lease_seconds),
        )
        .await
        .map(|mut batch| batch.artifacts.remove(0)),
    }
}

/// Longest wait a rate limit may impose on the next claim. GitHub's primary
/// quota resets within the hour; a larger (or hostile) reset time must not
/// park a record — which keeps owning its snapshot's articles — for longer.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::hours(1);

/// R1-P-S12: GitHub asks to wait until `reset_at` (Retry-After or quota
/// reset) before trying again; persisted so the next claim honours it.
/// Capped at `now + MAX_RATE_LIMIT_WAIT`; `None` for other errors and for a
/// reset that is not in the future (no wait to honour — persisting `now`
/// would only make a same-millisecond re-claim miss on SQLite's rounding).
/// Also applies to the rate limit an unverified write's HEAD verification ran
/// into (R3F-B2, [`PublishError::rate_limit_reset`]).
fn rate_limit_retry_not_before(
    error: &PublishError,
    now: OffsetDateTime,
) -> Option<OffsetDateTime> {
    let reset_at = error.rate_limit_reset()?;
    if reset_at <= now {
        return None;
    }
    let latest = now.checked_add(MAX_RATE_LIMIT_WAIT).unwrap_or(now);
    Some(reset_at.min(latest))
}

/// An attempt released without charging the stage budget — the record keeps
/// its state and its snapshot's articles, the claim is refunded and only
/// `next_attempt_at` (if any) delays the next claim:
/// - R3F-B2: a write whose HEAD verification did not complete;
/// - R4-1: a record whose earlier write is still unverified;
/// - R3F-S12 / R4-4: every rate limit, whether or not it leaves a future wait
///   to persist. GitHub refused the request; that is not a failed attempt, so
///   even the last budget must not fold the record to `failed`. A deadline
///   already past (`Retry-After: 0`, or one that elapsed before the release)
///   stores no wait; the record is retried at the next run, one attempt per
///   run, with no request resent inside a claim (R3F-B1).
fn is_deferred_retry(error: &PublishError) -> bool {
    matches!(
        error,
        PublishError::RemoteWriteUnverified { .. }
            | PublishError::EarlierWriteUnverified { .. }
            | PublishError::GitHubRateLimit { .. }
    )
}

enum RemotePreparationError {
    Report(ReportError),
    Timeout,
}

async fn prepare_remote_report(
    item_repo: &dyn PublishItemRepository,
    claimed: &ClaimedPublishRecord,
    config: &RenderConfig,
    deadline: Instant,
    emitter: &RunEventEmitter<'_>,
    phase: &'static str,
) -> Result<(RenderedReport, Vec<i64>), RemotePreparationError> {
    // A stalled event insert or snapshot read consumes the same lease budget
    // as the remote call. A timed-out preparation must remain retryable.
    if Instant::now() >= deadline {
        return Err(RemotePreparationError::Timeout);
    }
    let prepared = tokio::time::timeout_at(deadline, async {
        emitter
            .emit(
                "publish_started",
                "info",
                Some("publish_record"),
                Some(claimed.id),
                "remote publish started",
                Some(json!({ "phase": phase })),
            )
            .await;
        let frozen = load_frozen_items(item_repo, claimed.id).await?;
        let report = render_markdown(
            claimed.id,
            &claimed.category_key,
            &claimed.report_date,
            &frozen,
            config,
        )?;
        let article_ids = frozen.into_iter().map(|item| item.article_id).collect();
        Ok((report, article_ids))
    })
    .await
    .map_err(|_| RemotePreparationError::Timeout)?
    .map_err(RemotePreparationError::Report)?;
    // Timeout cannot preempt synchronous rendering inside a single future poll.
    if Instant::now() >= deadline {
        return Err(RemotePreparationError::Timeout);
    }
    Ok(prepared)
}

/// Target drift guard: a `stored_local` record carries the destination it was
/// initialised for (`owner/repo:branch`). If the configured target now
/// declares a different identity, sending would publish the frozen snapshot to
/// a repository/branch nobody chose for it, so the stage refuses before any
/// request. Targets without an identity are not compared.
fn target_drift(
    claimed: &ClaimedPublishRecord,
    target: &dyn PublishTarget,
) -> Option<PublishError> {
    let current = target.target_identity()?;
    if claimed.remote_target.as_deref() == Some(current.as_str()) {
        None
    } else {
        Some(PublishError::RemoteTargetMismatch {
            stored: claimed.remote_target.clone(),
            current,
        })
    }
}

fn mismatch_status(error: &PublishError) -> PublishRemoteStatus {
    match error {
        PublishError::RemoteTargetMismatch { stored, current } => {
            PublishRemoteStatus::TargetMismatch {
                stored: stored.clone(),
                current: current.clone(),
            }
        }
        other => PublishRemoteStatus::Failed {
            error_kind: other.error_kind().to_string(),
        },
    }
}

impl PublishFlow {
    pub async fn publish_remote(&self, opts: PublishRemoteOptions) -> PublishRemoteOutcome {
        self.publish_remote_inner(None, opts).await
    }

    /// 只领取指定 `publish_record_id`（且处于 `StoredLocal`）的记录；单分类
    /// `publish` 命令使用，避免全库 claim 领到其他分类的记录。
    pub async fn publish_remote_record(
        &self,
        publish_record_id: i64,
        opts: PublishRemoteOptions,
    ) -> PublishRemoteOutcome {
        self.publish_remote_inner(Some(publish_record_id), opts)
            .await
    }

    async fn publish_remote_inner(
        &self,
        publish_record_id: Option<i64>,
        opts: PublishRemoteOptions,
    ) -> PublishRemoteOutcome {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run.run_id,
            stage: "publish",
            repo: self.ctx.event_repo.as_ref(),
        };
        let target = match self.ctx.publish_target_remote.as_ref() {
            Some(target) => Arc::clone(target),
            None => {
                tracing::warn!("publish_remote called without publish_target_remote configured");
                return PublishRemoteOutcome {
                    publish_record_id: 0,
                    status: PublishRemoteStatus::MissingTarget,
                    commit_sha: None,
                    remote_target: None,
                    item_count: 0,
                };
            }
        };

        // W15 §5：publish CLI 断点续跑时 publish_remote 可能是本次 run 的
        // 首次 claim（record 停在 stored_local），同样接 ① + ②（codex
        // W15-P4 复审）。
        self.run_publish_maintenance(&emitter).await;

        let deadline = remote_deadline(self.ctx.lease.publish_duration_seconds);
        let now = OffsetDateTime::now_utc();
        let owner = build_owner_id();
        let claim = ClaimRequest {
            owner: owner.clone(),
            now,
            lease_expires_at: lease_expires_at(
                now,
                Duration::seconds(self.ctx.lease.publish_duration_seconds as i64),
            ),
            batch_size: 1,
            max_attempts: self.ctx.retry.publish_max_attempts,
        };
        let claimed_result = if let Some(publish_record_id) = publish_record_id {
            self.ctx
                .publish_record_repo
                .claim_local_for_remote_publish_by_ids(&claim, &[publish_record_id])
                .await
        } else {
            self.ctx
                .publish_record_repo
                .claim_local_for_remote_publish(&claim)
                .await
        };
        let claimed = match claimed_result {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::error!("claim local for remote publish failed: {error}");
                return PublishRemoteOutcome {
                    publish_record_id: 0,
                    status: PublishRemoteStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    commit_sha: None,
                    remote_target: None,
                    item_count: 0,
                };
            }
        };
        let claimed = match claimed.into_iter().next() {
            Some(claimed) => claimed,
            None => {
                return PublishRemoteOutcome {
                    publish_record_id: 0,
                    status: PublishRemoteStatus::NothingToClaim,
                    commit_sha: None,
                    remote_target: None,
                    item_count: 0,
                };
            }
        };

        if let Some(error) = target_drift(&claimed, target.as_ref()) {
            self.release_publish_error_for_record(
                claimed.id,
                &owner,
                &error,
                None,
                "publish_remote",
                &emitter,
            )
            .await;
            return PublishRemoteOutcome {
                publish_record_id: claimed.id,
                status: mismatch_status(&error),
                commit_sha: None,
                remote_target: None,
                item_count: 0,
            };
        }

        let unverified_write = has_unverified_write(&claimed);
        if let Some(status) = self
            .remote_ownership_status(
                claimed.id,
                &owner,
                unverified_write,
                "publish_remote",
                &emitter,
            )
            .await
        {
            return PublishRemoteOutcome {
                publish_record_id: claimed.id,
                status,
                commit_sha: None,
                remote_target: None,
                item_count: 0,
            };
        }

        let render_config = RenderConfig {
            category_display_name: opts.category_display_name,
            report_title: opts.report_title,
            generated_at: rendered_timestamp(claimed.rendered_at, opts.generated_at),
            templates: render_templates_from_ctx(&self.ctx, opts.path_template.as_deref()),
        };
        let (report, promote_article_ids) = match prepare_remote_report(
            self.ctx.publish_item_repo.as_ref(),
            &claimed,
            &render_config,
            deadline,
            &emitter,
            "publish_remote",
        )
        .await
        {
            Ok(report) => report,
            Err(error) => {
                let error_kind = self
                    .release_preparation_error(
                        claimed.id,
                        &owner,
                        &error,
                        unverified_write,
                        &emitter,
                    )
                    .await;
                return PublishRemoteOutcome {
                    publish_record_id: claimed.id,
                    status: PublishRemoteStatus::Failed { error_kind },
                    commit_sha: None,
                    remote_target: None,
                    item_count: 0,
                };
            }
        };

        let item_count = promote_article_ids.len() as u32;

        let lease_seconds = self.ctx.lease.publish_duration_seconds;
        // R5-2: what a write of this report puts at the destination now.
        let written = target.remote_blob(&report);
        // Gates in order: R4-1 recovery check (read-only, before any write)
        // → R5-4 hold on a changed destination → R5-5 destination held by
        // another category → R5-3 record the write → send.
        let settled = if unverified_write {
            let recorded = claimed.unverified_write();
            match check_earlier_write(target.as_ref(), &report, recorded.as_ref(), deadline).await {
                Ok(Some(found)) => Some(Ok(found)),
                Ok(None) => destination_changed(recorded.as_ref(), written.as_ref()).map(Err),
                Err(error) => Some(Err(error)),
            }
        } else {
            None
        };
        let result = match settled {
            Some(result) => result,
            None => {
                if let Err(status) = self
                    .gate_remote_write(
                        &claimed,
                        &owner,
                        target.as_ref(),
                        written.as_ref(),
                        "publish_remote",
                        &emitter,
                    )
                    .await
                {
                    return PublishRemoteOutcome {
                        publish_record_id: claimed.id,
                        status,
                        commit_sha: None,
                        remote_target: None,
                        item_count,
                    };
                }
                send_report(target.as_ref(), &report, deadline, lease_seconds)
                    .await
                    .map(|artifact| (artifact, written.clone()))
            }
        };
        let now = OffsetDateTime::now_utc();
        let (artifact, receipt_blob) = match result {
            Ok(found) => found,
            Err(error) => {
                self.release_publish_error_for_record(
                    claimed.id,
                    &owner,
                    &error,
                    written.as_ref(),
                    "publish_remote",
                    &emitter,
                )
                .await;
                return PublishRemoteOutcome {
                    publish_record_id: claimed.id,
                    status: PublishRemoteStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    commit_sha: None,
                    remote_target: None,
                    item_count,
                };
            }
        };

        let status = self
            .advance_published_remote(
                claimed.id,
                &owner,
                promote_article_ids,
                &artifact,
                receipt_blob.as_ref(),
                item_count,
                "publish_remote",
                "published remotely",
                now,
                &emitter,
            )
            .await;
        PublishRemoteOutcome {
            publish_record_id: claimed.id,
            status,
            commit_sha: artifact.commit_sha,
            remote_target: artifact.remote_target,
            item_count,
        }
    }

    pub async fn publish_remote_batch(
        &self,
        opts: PublishRemoteBatchOptions,
    ) -> PublishRemoteBatchOutcome {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run.run_id,
            stage: "publish",
            repo: self.ctx.event_repo.as_ref(),
        };
        let target = match self.ctx.publish_target_remote.as_ref() {
            Some(target) => Arc::clone(target),
            None => {
                tracing::warn!(
                    "publish_remote_batch called without publish_target_remote configured"
                );
                return PublishRemoteBatchOutcome {
                    commit_sha: None,
                    items: opts
                        .items
                        .into_iter()
                        .map(|item| PublishRemoteOutcome {
                            publish_record_id: item.publish_record_id,
                            status: PublishRemoteStatus::MissingTarget,
                            commit_sha: None,
                            remote_target: None,
                            item_count: 0,
                        })
                        .collect(),
                };
            }
        };
        if opts.items.is_empty() {
            return PublishRemoteBatchOutcome {
                items: Vec::new(),
                commit_sha: None,
            };
        }
        // One claim serves every item with its id, so a repeated id would
        // send one snapshot twice (e.g. under two path templates). A caller
        // bug: nothing is claimed or sent.
        let mut seen = std::collections::HashSet::new();
        if !opts
            .items
            .iter()
            .all(|item| seen.insert(item.publish_record_id))
        {
            tracing::error!("publish_remote_batch called with a repeated publish_record_id");
            return PublishRemoteBatchOutcome {
                commit_sha: None,
                items: opts
                    .items
                    .into_iter()
                    .map(|item| PublishRemoteOutcome {
                        publish_record_id: item.publish_record_id,
                        status: PublishRemoteStatus::Failed {
                            error_kind: DUPLICATE_BATCH_RECORD_KIND.to_string(),
                        },
                        commit_sha: None,
                        remote_target: None,
                        item_count: 0,
                    })
                    .collect(),
            };
        }

        // W15 §5：remote batch 是独立 CLI 入口（publish_all 跨类目第二阶段），
        // 首次 claim 前执行一次 ① reclaim + ② sweep（顺序固定，best-effort）。
        self.run_publish_maintenance(&emitter).await;

        let deadline = remote_deadline(self.ctx.lease.publish_duration_seconds);
        let now = OffsetDateTime::now_utc();
        let owner = build_owner_id();
        let ids = opts
            .items
            .iter()
            .map(|item| item.publish_record_id)
            .collect::<Vec<_>>();
        let claim = ClaimRequest {
            owner: owner.clone(),
            now,
            lease_expires_at: lease_expires_at(
                now,
                Duration::seconds(self.ctx.lease.publish_duration_seconds as i64),
            ),
            batch_size: ids.len() as u32,
            max_attempts: self.ctx.retry.publish_max_attempts,
        };
        let claimed = match self
            .ctx
            .publish_record_repo
            .claim_local_for_remote_publish_by_ids(&claim, &ids)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::error!("claim local publish records by ids failed: {error}");
                return PublishRemoteBatchOutcome {
                    commit_sha: None,
                    items: opts
                        .items
                        .into_iter()
                        .map(|item| PublishRemoteOutcome {
                            publish_record_id: item.publish_record_id,
                            status: PublishRemoteStatus::Failed {
                                error_kind: error.error_kind().to_string(),
                            },
                            commit_sha: None,
                            remote_target: None,
                            item_count: 0,
                        })
                        .collect(),
                };
            }
        };
        let claimed_by_id = claimed
            .into_iter()
            .map(|claimed| (claimed.id, claimed))
            .collect::<HashMap<_, _>>();

        let mut prepared = Vec::new();
        let mut outcomes = Vec::with_capacity(opts.items.len());
        for item in opts.items {
            let claimed = match claimed_by_id.get(&item.publish_record_id) {
                Some(claimed) => claimed,
                None => {
                    outcomes.push(PublishRemoteOutcome {
                        publish_record_id: item.publish_record_id,
                        status: PublishRemoteStatus::NothingToClaim,
                        commit_sha: None,
                        remote_target: None,
                        item_count: 0,
                    });
                    continue;
                }
            };

            if let Some(error) = target_drift(claimed, target.as_ref()) {
                self.release_publish_error_for_record(
                    claimed.id,
                    &owner,
                    &error,
                    None,
                    "publish_remote_batch",
                    &emitter,
                )
                .await;
                outcomes.push(PublishRemoteOutcome {
                    publish_record_id: claimed.id,
                    status: mismatch_status(&error),
                    commit_sha: None,
                    remote_target: None,
                    item_count: 0,
                });
                continue;
            }

            let unverified_write = has_unverified_write(claimed);
            // Only the losing record leaves the batch; the rest still commit.
            if let Some(status) = self
                .remote_ownership_status(
                    claimed.id,
                    &owner,
                    unverified_write,
                    "publish_remote_batch",
                    &emitter,
                )
                .await
            {
                outcomes.push(PublishRemoteOutcome {
                    publish_record_id: claimed.id,
                    status,
                    commit_sha: None,
                    remote_target: None,
                    item_count: 0,
                });
                continue;
            }

            let render_config = RenderConfig {
                category_display_name: item.category_display_name,
                report_title: item.report_title,
                generated_at: rendered_timestamp(claimed.rendered_at, item.generated_at),
                templates: render_templates_from_ctx(&self.ctx, item.path_template.as_deref()),
            };
            let (report, promote_article_ids) = match prepare_remote_report(
                self.ctx.publish_item_repo.as_ref(),
                claimed,
                &render_config,
                deadline,
                &emitter,
                "publish_remote_batch",
            )
            .await
            {
                Ok(report) => report,
                Err(error) => {
                    let error_kind = self
                        .release_preparation_error(
                            claimed.id,
                            &owner,
                            &error,
                            unverified_write,
                            &emitter,
                        )
                        .await;
                    outcomes.push(PublishRemoteOutcome {
                        publish_record_id: claimed.id,
                        status: PublishRemoteStatus::Failed { error_kind },
                        commit_sha: None,
                        remote_target: None,
                        item_count: 0,
                    });
                    continue;
                }
            };

            let item_count = promote_article_ids.len() as u32;
            // R4-1: each record with an unverified earlier write is checked on
            // its own (`find_published` is all-or-nothing over its input, so a
            // sibling's absence must not hide this record's landed write).
            // Found → recorded from its receipt and left out of the batch
            // write; check failed → deferred; confirmed absent → written with
            // the batch.
            if unverified_write {
                let recorded = claimed.unverified_write();
                match check_earlier_write(target.as_ref(), &report, recorded.as_ref(), deadline)
                    .await
                {
                    Ok(Some((artifact, receipt_blob))) => {
                        let status = self
                            .advance_published_remote(
                                claimed.id,
                                &owner,
                                promote_article_ids,
                                &artifact,
                                receipt_blob.as_ref(),
                                item_count,
                                "publish_remote_batch",
                                "earlier remote write recorded in batch",
                                OffsetDateTime::now_utc(),
                                &emitter,
                            )
                            .await;
                        outcomes.push(PublishRemoteOutcome {
                            publish_record_id: claimed.id,
                            status,
                            commit_sha: artifact.commit_sha,
                            remote_target: artifact.remote_target,
                            item_count,
                        });
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.release_publish_error_for_record(
                            claimed.id,
                            &owner,
                            &error,
                            None,
                            "publish_remote_batch",
                            &emitter,
                        )
                        .await;
                        outcomes.push(PublishRemoteOutcome {
                            publish_record_id: claimed.id,
                            status: PublishRemoteStatus::Failed {
                                error_kind: error.error_kind().to_string(),
                            },
                            commit_sha: None,
                            remote_target: None,
                            item_count,
                        });
                        continue;
                    }
                }
                // R5-4: confirmed absent, but this batch would write elsewhere.
                if let Some(hold) =
                    destination_changed(recorded.as_ref(), target.remote_blob(&report).as_ref())
                {
                    self.release_publish_error_for_record(
                        claimed.id,
                        &owner,
                        &hold,
                        None,
                        "publish_remote_batch",
                        &emitter,
                    )
                    .await;
                    outcomes.push(PublishRemoteOutcome {
                        publish_record_id: claimed.id,
                        status: PublishRemoteStatus::Failed {
                            error_kind: hold.error_kind().to_string(),
                        },
                        commit_sha: None,
                        remote_target: None,
                        item_count,
                    });
                    continue;
                }
            }
            let blob = target.remote_blob(&report);
            prepared.push(PreparedRemote {
                publish_record_id: claimed.id,
                promote_article_ids,
                item_count,
                blob,
                report,
            });
        }

        // R5-5 / R5-3 after every member was checked: the records this batch
        // recovered or left unverified are on record by now, so a member
        // aimed at one of their destinations is held; the rest record their
        // write before it is sent. A member stopped here leaves the batch
        // (its report with it — `prepared` is the batch, in order).
        let mut ready = Vec::with_capacity(prepared.len());
        for item in prepared {
            let Some(claimed) = claimed_by_id.get(&item.publish_record_id) else {
                continue;
            };
            match self
                .gate_remote_write(
                    claimed,
                    &owner,
                    target.as_ref(),
                    item.blob.as_ref(),
                    "publish_remote_batch",
                    &emitter,
                )
                .await
            {
                Ok(()) => ready.push(item),
                Err(status) => outcomes.push(PublishRemoteOutcome {
                    publish_record_id: item.publish_record_id,
                    status,
                    commit_sha: None,
                    remote_target: None,
                    item_count: item.item_count,
                }),
            }
        }
        let prepared = ready;
        if prepared.is_empty() {
            return PublishRemoteBatchOutcome {
                items: outcomes,
                commit_sha: None,
            };
        }
        let reports = prepared
            .iter()
            .map(|item| item.report.clone())
            .collect::<Vec<_>>();

        let result = if Instant::now() >= deadline {
            Err(PublishError::RemoteTimeout)
        } else {
            match tokio::time::timeout_at(deadline, target.publish_many(&reports)).await {
                Ok(result) => result,
                // The cancelled call may already have written.
                Err(_) => {
                    verify_after_timeout(
                        target.as_ref(),
                        &reports,
                        verify_deadline(deadline, self.ctx.lease.publish_duration_seconds),
                    )
                    .await
                }
            }
        };
        let now = OffsetDateTime::now_utc();
        let batch = match result {
            Ok(batch) if batch.artifacts.len() == prepared.len() => batch,
            Ok(batch) => {
                let error = PublishError::api(
                    502,
                    format!(
                        "publish_many returned {} artifacts for {} reports",
                        batch.artifacts.len(),
                        prepared.len()
                    ),
                );
                self.release_publish_error_for_prepared(&prepared, &owner, &error, &emitter)
                    .await;
                for item in prepared {
                    outcomes.push(PublishRemoteOutcome {
                        publish_record_id: item.publish_record_id,
                        status: PublishRemoteStatus::Failed {
                            error_kind: error.error_kind().to_string(),
                        },
                        commit_sha: None,
                        remote_target: None,
                        item_count: item.item_count,
                    });
                }
                return PublishRemoteBatchOutcome {
                    items: outcomes,
                    commit_sha: None,
                };
            }
            Err(error) => {
                self.release_publish_error_for_prepared(&prepared, &owner, &error, &emitter)
                    .await;
                for item in prepared {
                    outcomes.push(PublishRemoteOutcome {
                        publish_record_id: item.publish_record_id,
                        status: PublishRemoteStatus::Failed {
                            error_kind: error.error_kind().to_string(),
                        },
                        commit_sha: None,
                        remote_target: None,
                        item_count: item.item_count,
                    });
                }
                return PublishRemoteBatchOutcome {
                    items: outcomes,
                    commit_sha: None,
                };
            }
        };

        for (item, artifact) in prepared.into_iter().zip(batch.artifacts) {
            let status = self
                .advance_published_remote(
                    item.publish_record_id,
                    &owner,
                    item.promote_article_ids,
                    &artifact,
                    item.blob.as_ref(),
                    item.item_count,
                    "publish_remote_batch",
                    "published remotely in batch",
                    now,
                    &emitter,
                )
                .await;
            outcomes.push(PublishRemoteOutcome {
                publish_record_id: item.publish_record_id,
                status,
                commit_sha: artifact.commit_sha,
                remote_target: artifact.remote_target,
                item_count: item.item_count,
            });
        }

        PublishRemoteBatchOutcome {
            items: outcomes,
            commit_sha: batch.commit_sha,
        }
    }

    /// The last gates before a remote write of `claimed`, after the R4-1
    /// check and the R5-4 hold; `Ok` = send now, `Err` = the attempt ended
    /// here (released) with that status.
    /// 1. R5-5: another category's record holds `written`'s destination —
    ///    published there, or its write there unsettled — so writing would
    ///    overwrite that report: held without a write (refunded, ownership
    ///    kept, `remote_path_conflict`) until the paths are fixed.
    /// 2. R5-3: record the write about to be sent (recovery kind, path,
    ///    blob), so a crash between the write and its outcome leaves a record
    ///    the next claim recovers at that path. A lost lease or a storage
    ///    error sends nothing.
    async fn gate_remote_write(
        &self,
        claimed: &ClaimedPublishRecord,
        owner: &str,
        target: &dyn PublishTarget,
        written: Option<&RemoteBlob>,
        phase: &'static str,
        emitter: &RunEventEmitter<'_>,
    ) -> Result<(), PublishRemoteStatus> {
        let now = OffsetDateTime::now_utc();
        if let Some(blob) = written
            && let Some(identity) = claimed.remote_target.as_deref()
            && let Some(receipt) = target.receipt_target(&blob.path)
        {
            match self
                .ctx
                .publish_record_repo
                .find_remote_destination_holder(
                    claimed.id,
                    &claimed.category_key,
                    &blob.path,
                    REMOTE_WRITE_UNVERIFIED_KIND,
                    identity,
                    &receipt,
                )
                .await
            {
                Ok(None) => {}
                Ok(Some(holder)) => {
                    self.release_path_conflict(
                        claimed.id, owner, &blob.path, holder, phase, emitter,
                    )
                    .await;
                    return Err(PublishRemoteStatus::Failed {
                        error_kind: REMOTE_PATH_CONFLICT_KIND.to_string(),
                    });
                }
                Err(error) => {
                    self.release_classified_failure(claimed.id, owner, &error, phase, now, emitter)
                        .await;
                    return Err(PublishRemoteStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    });
                }
            }
        }
        match self
            .ctx
            .publish_record_repo
            .mark_remote_write_pending(
                claimed.id,
                owner,
                PublishState::StoredLocal,
                PENDING_WRITE_NOTE,
                REMOTE_WRITE_UNVERIFIED_KIND,
                written,
                now,
            )
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => {
                tracing::warn!(
                    publish_record_id = claimed.id,
                    phase,
                    "lease lost before the remote write; nothing sent"
                );
                Err(PublishRemoteStatus::Conflicted)
            }
            Err(error) => {
                self.release_classified_failure(claimed.id, owner, &error, phase, now, emitter)
                    .await;
                Err(PublishRemoteStatus::Failed {
                    error_kind: error.error_kind().to_string(),
                })
            }
        }
    }

    /// R5-5: releases a write held because record `holder` (another
    /// category) holds its destination `path`: no write, claim refunded,
    /// state and ownership kept; any recorded write is cleared (the recovery
    /// check confirmed it absent, or there was none).
    async fn release_path_conflict(
        &self,
        publish_record_id: i64,
        owner: &str,
        path: &str,
        holder: i64,
        phase: &'static str,
        emitter: &RunEventEmitter<'_>,
    ) {
        let now = OffsetDateTime::now_utc();
        let message = format!(
            "remote path {path} is held by publish record {holder} of another category \
             (published there, or its write there is unverified); record {publish_record_id} \
             sends nothing until the path templates or the prefix stop sharing it"
        );
        let released = self
            .ctx
            .publish_record_repo
            .release_deferred_retry(
                publish_record_id,
                owner,
                PublishState::StoredLocal,
                &message,
                REMOTE_PATH_CONFLICT_KIND,
                None,
                UnverifiedWriteUpdate::Record(None),
                now,
            )
            .await;
        if let Err(persist_err) = &released {
            tracing::warn!(
                publish_record_id,
                phase,
                ?persist_err,
                "path-conflict release could not be persisted; lease recovery remains available",
            );
        }
        emitter
            .emit(
                "publish_failed",
                "error",
                Some("publish_record"),
                Some(publish_record_id),
                &message,
                Some(json!({
                    "phase": phase,
                    "error_kind": REMOTE_PATH_CONFLICT_KIND,
                    "path": path,
                    "holder_record_id": holder,
                    "budget_exhausted": false,
                    "attempt_refunded": matches!(released, Ok(true)),
                })),
            )
            .await;
    }

    /// Pre-write ownership guard for a claimed `stored_local` record (P-B1);
    /// `None` = clear to send.
    async fn remote_ownership_status(
        &self,
        publish_record_id: i64,
        owner: &str,
        unverified_write: bool,
        phase: &'static str,
        emitter: &RunEventEmitter<'_>,
    ) -> Option<PublishRemoteStatus> {
        match self
            .guard_snapshot_ownership(
                publish_record_id,
                owner,
                PublishState::StoredLocal,
                unverified_write,
                phase,
                emitter,
            )
            .await
        {
            OwnershipGuard::Clear => None,
            OwnershipGuard::Conflict { article_id } => {
                Some(PublishRemoteStatus::ArticleConflict { article_id })
            }
            OwnershipGuard::Failed { error_kind } => {
                Some(PublishRemoteStatus::Failed { error_kind })
            }
        }
    }

    /// Terminal advance `stored_local → published_remote` with the receipt of
    /// `artifact`, promoting the snapshot's articles; emits
    /// `publish_succeeded` (`message`) on success. `receipt_blob` = the blob
    /// the receipt is for, recorded if the advance fails (R5-1 / R5-2).
    #[allow(clippy::too_many_arguments)]
    async fn advance_published_remote(
        &self,
        publish_record_id: i64,
        owner: &str,
        promote_article_ids: Vec<i64>,
        artifact: &PublishedArtifact,
        receipt_blob: Option<&RemoteBlob>,
        item_count: u32,
        phase: &'static str,
        message: &str,
        now: OffsetDateTime,
        emitter: &RunEventEmitter<'_>,
    ) -> PublishRemoteStatus {
        let extras = PublishAdvanceExtras {
            local_path: None,
            remote_target: artifact.remote_target.clone(),
            commit_sha: artifact.commit_sha.clone(),
        };
        match self
            .ctx
            .publish_record_repo
            .release_terminal_advance_with_articles(
                publish_record_id,
                owner,
                PublishState::StoredLocal,
                PublishState::PublishedRemote,
                PublishTimestampField::RemotePublishedAt,
                promote_article_ids,
                extras,
                now,
            )
            .await
        {
            Ok(TerminalAdvanceOutcome {
                status: TerminalAdvanceStatus::Advanced,
            }) => {
                emitter
                    .emit(
                        "publish_succeeded",
                        "info",
                        Some("publish_record"),
                        Some(publish_record_id),
                        message,
                        Some(json!({
                            "phase": phase,
                            "commit_sha": artifact.commit_sha.as_deref(),
                            "remote_target": artifact.remote_target.as_deref(),
                            "item_count": item_count
                        })),
                    )
                    .await;
                PublishRemoteStatus::PublishedRemote
            }
            Ok(TerminalAdvanceOutcome {
                status: TerminalAdvanceStatus::PublishRecordConflict,
            }) => PublishRemoteStatus::Conflicted,
            Ok(TerminalAdvanceOutcome {
                status: TerminalAdvanceStatus::ArticleStateConflict { article_id },
            }) => PublishRemoteStatus::ArticleConflict { article_id },
            // R5-1: every caller holds a receipt, so the destination has the
            // report; only its record is missing. Left leased, the record
            // would be reclaimed and — on the last stage budget — swept to
            // `failed`, releasing articles that are already published. It is
            // deferred instead, marked for recovery: the next claim finds the
            // report with a read-only check and records the receipt without a
            // write. This holds for non-retryable storage errors too: once the
            // destination holds the report, `failed` is never the right end.
            // R5-2: the blob behind the receipt is recorded, so that check
            // looks where this report is even if the paths change meanwhile.
            Err(error) => {
                self.release_unverified_recovery(
                    publish_record_id,
                    owner,
                    &error,
                    RECEIPT_NOT_RECORDED,
                    UnverifiedWriteUpdate::Record(receipt_blob),
                    phase,
                    emitter,
                )
                .await;
                PublishRemoteStatus::Failed {
                    error_kind: REMOTE_WRITE_UNVERIFIED_KIND.to_string(),
                }
            }
        }
    }

    /// Releases a record whose remote outcome is not on record, without
    /// charging the stage: state kept, claim refunded,
    /// `last_error_kind = remote_write_unverified` so the next claim starts
    /// with the read-only check. `reason` opens `last_error`:
    /// - [`EARLIER_WRITE_UNVERIFIED`] (R4-1): a retryable failure (`cause`: a
    ///   snapshot or ownership read) before the HEAD check of a record whose
    ///   earlier write is unverified — nothing written, nothing settled;
    /// - [`RECEIPT_NOT_RECORDED`] (R5-1): the destination holds the report
    ///   but persisting its receipt failed (`cause`).
    ///
    /// `unverified_write` (R5-2): `Keep` for the former (the record keeps
    /// checking the write it inherited), `Record` of the receipt's blob for
    /// the latter.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn release_unverified_recovery(
        &self,
        publish_record_id: i64,
        owner: &str,
        cause: &dyn ClassifiedError,
        reason: &str,
        unverified_write: UnverifiedWriteUpdate<'_>,
        phase: &'static str,
        emitter: &RunEventEmitter<'_>,
    ) {
        let now = OffsetDateTime::now_utc();
        let message = format!("{reason}: {}", cause.display_user());
        let released = self
            .ctx
            .publish_record_repo
            .release_deferred_retry(
                publish_record_id,
                owner,
                PublishState::StoredLocal,
                &message,
                REMOTE_WRITE_UNVERIFIED_KIND,
                None,
                unverified_write,
                now,
            )
            .await;
        if let Err(persist_err) = &released {
            tracing::warn!(
                publish_record_id,
                phase,
                ?persist_err,
                "deferred release could not be persisted; lease recovery remains available",
            );
        }
        emitter
            .emit(
                "publish_failed",
                "error",
                Some("publish_record"),
                Some(publish_record_id),
                &message,
                Some(json!({
                    "phase": phase,
                    "error_kind": REMOTE_WRITE_UNVERIFIED_KIND,
                    "cause_kind": cause.error_kind(),
                    "budget_exhausted": false,
                    "attempt_refunded": matches!(released, Ok(true)),
                })),
            )
            .await;
    }

    async fn release_publish_error_for_prepared(
        &self,
        prepared: &[PreparedRemote],
        owner: &str,
        error: &PublishError,
        emitter: &RunEventEmitter<'_>,
    ) {
        for item in prepared {
            self.release_publish_error_for_record(
                item.publish_record_id,
                owner,
                error,
                item.blob.as_ref(),
                "publish_remote_batch",
                emitter,
            )
            .await;
        }
    }

    /// `written` = the blob this attempt's write put (or would have put) at
    /// the destination; a deferred release records or keeps the unverified
    /// write per [`unverified_write_update`] (R5-2).
    async fn release_publish_error_for_record(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &PublishError,
        written: Option<&RemoteBlob>,
        phase: &'static str,
        emitter: &RunEventEmitter<'_>,
    ) {
        let now = OffsetDateTime::now_utc();
        let message = error.display_user();
        let retry_not_before = rate_limit_retry_not_before(error, now);
        let mut budget_exhausted = false;
        let deferred = is_deferred_retry(error);
        let release_result = if deferred {
            // Every caller releases a record claimed for the remote stage.
            self.ctx
                .publish_record_repo
                .release_deferred_retry(
                    publish_record_id,
                    owner,
                    PublishState::StoredLocal,
                    &message,
                    error.error_kind(),
                    retry_not_before,
                    unverified_write_update(error, written),
                    now,
                )
                .await
        } else if error.is_retryable() {
            self.ctx
                .publish_record_repo
                .release_retryable_failure_until(
                    publish_record_id,
                    owner,
                    &message,
                    error.error_kind(),
                    self.ctx.retry.publish_max_attempts,
                    retry_not_before,
                    now,
                )
                .await
                .map(|outcome| {
                    budget_exhausted = outcome.exhausted;
                    outcome.released
                })
        } else {
            self.ctx
                .publish_record_repo
                .release_permanent_failure(
                    publish_record_id,
                    owner,
                    &message,
                    error.error_kind(),
                    now,
                )
                .await
        };
        // R4-7: only a deferred release that updated the row refunded the
        // claim (`Ok(false)` = the lease was lost, nothing changed).
        let attempt_refunded = deferred && matches!(release_result, Ok(true));
        if let Err(persist_err) = release_result {
            tracing::warn!(
                publish_record_id,
                phase,
                retryable = error.is_retryable(),
                ?persist_err,
                "release failure could not be persisted; lease recovery remains available",
            );
        }
        emitter
            .emit(
                "publish_failed",
                "error",
                Some("publish_record"),
                Some(publish_record_id),
                &message,
                Some(json!({
                    "phase": phase,
                    "error_kind": error.error_kind(),
                    "budget_exhausted": budget_exhausted,
                    // R3F-B2 / S12: released without charging the stage budget.
                    "attempt_refunded": attempt_refunded,
                    // R1-P-S1: where a GitHub failure happened, as fields.
                    "operation": error.github_context().and_then(|context| context.operation),
                    "http_status": error.github_context().and_then(|context| context.http_status),
                    "request_id": error
                        .github_context()
                        .and_then(|context| context.request_id.as_deref()),
                    "retry_not_before": retry_not_before.map(|at| at.to_string()),
                    // R5-4: a recovery held on a changed destination.
                    "recorded_path": error.destination_change().map(|(recorded, _)| recorded),
                    "current_path": error.destination_change().and_then(|(_, current)| current),
                })),
            )
            .await;
    }

    /// Releases a failed preparation and returns the `error_kind` it was
    /// released with. With `unverified_write` (R4-1) a retryable failure
    /// is deferred under `remote_write_unverified` (nothing written, nothing
    /// settled); a permanent report error is released as usual.
    async fn release_preparation_error(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &RemotePreparationError,
        unverified_write: bool,
        emitter: &RunEventEmitter<'_>,
    ) -> String {
        match error {
            RemotePreparationError::Report(error) if unverified_write && error.is_retryable() => {
                self.release_unverified_recovery(
                    publish_record_id,
                    owner,
                    error,
                    EARLIER_WRITE_UNVERIFIED,
                    UnverifiedWriteUpdate::Keep,
                    "publish_remote.prepare",
                    emitter,
                )
                .await;
                REMOTE_WRITE_UNVERIFIED_KIND.to_string()
            }
            RemotePreparationError::Report(error) => {
                self.release_report_error(publish_record_id, owner, error, emitter)
                    .await;
                error.error_kind().to_string()
            }
            RemotePreparationError::Timeout => {
                let error = if unverified_write {
                    PublishError::EarlierWriteUnverified {
                        verification: Box::new(PublishError::RemoteTimeout),
                    }
                } else {
                    PublishError::RemoteTimeout
                };
                self.release_publish_error_for_record(
                    publish_record_id,
                    owner,
                    &error,
                    None,
                    "publish_remote.prepare",
                    emitter,
                )
                .await;
                error.error_kind().to_string()
            }
        }
    }

    async fn release_report_error(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &ReportError,
        emitter: &RunEventEmitter<'_>,
    ) {
        // A snapshot read that failed transiently stays `stored_local`.
        self.release_classified_failure(
            publish_record_id,
            owner,
            error,
            "publish_remote",
            OffsetDateTime::now_utc(),
            emitter,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_deadline_reserves_subsecond_release_time() {
        let before = Instant::now();
        let deadline = remote_deadline(1);
        let after = Instant::now();
        let budget = StdDuration::from_millis(800);
        assert!(deadline >= before + budget);
        assert!(deadline <= after + budget);
        assert!(remote_deadline(0) <= Instant::now());
    }

    /// R5-2: a fresh unverified write is recorded, an unsettled recovery
    /// keeps the write it inherited, and a rate limit (nothing unsettled)
    /// clears it.
    #[test]
    fn deferred_releases_record_keep_or_clear_the_unverified_write() {
        let written = RemoteBlob {
            path: "reports/ai.md".to_string(),
            blob_sha: "sha".to_string(),
        };
        let fresh = PublishError::RemoteWriteUnverified {
            write: Box::new(PublishError::RemoteTimeout),
            verification: Box::new(PublishError::RemoteTimeout),
        };
        assert!(matches!(
            unverified_write_update(&fresh, Some(&written)),
            UnverifiedWriteUpdate::Record(Some(blob)) if blob == &written
        ));
        let unsettled = PublishError::EarlierWriteUnverified {
            verification: Box::new(PublishError::RemoteTimeout),
        };
        assert!(matches!(
            unverified_write_update(&unsettled, Some(&written)),
            UnverifiedWriteUpdate::Keep
        ));
        let rate_limited = PublishError::rate_limit(OffsetDateTime::now_utc());
        assert!(matches!(
            unverified_write_update(&rate_limited, Some(&written)),
            UnverifiedWriteUpdate::Record(None)
        ));
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use rss_ai_news_storage::*;

    struct ReadyAfterDeadline(Instant);
    #[async_trait::async_trait]
    impl PublishItemRepository for ReadyAfterDeadline {
        async fn select_ai_path_candidates(
            &self,
            _: &str,
            _: i32,
            _: OffsetDateTime,
            _: OffsetDateTime,
            _: std::num::NonZeroU32,
        ) -> Result<Vec<PublishCandidateRow>, StorageError> {
            unreachable!()
        }
        async fn select_ai_off_passthrough_candidates(
            &self,
            _: &str,
            _: OffsetDateTime,
            _: OffsetDateTime,
            _: std::num::NonZeroU32,
        ) -> Result<Vec<PublishCandidateRow>, StorageError> {
            unreachable!()
        }
        async fn freeze_snapshot(
            &self,
            _: i64,
            _: &str,
            _: Vec<FreezeSnapshotItem>,
            _: Vec<i64>,
            _: OffsetDateTime,
        ) -> Result<FreezeSnapshotOutcome, StorageError> {
            unreachable!()
        }
        async fn list_by_publish_record(
            &self,
            _: i64,
        ) -> Result<Vec<rss_ai_news_domain::model::PublishItem>, StorageError> {
            // Reproduce a single poll that consumes the remaining budget without
            // yielding, as synchronous rendering can do. No polling timer race.
            while Instant::now() < self.0 {
                std::thread::sleep(self.0.saturating_duration_since(Instant::now()));
            }
            Ok(vec![])
        }
        async fn find_snapshot_ownership_conflict(
            &self,
            _: i64,
        ) -> Result<Option<SnapshotOwnershipConflict>, StorageError> {
            unreachable!()
        }
    }
    struct NoopEvents;
    #[async_trait::async_trait]
    impl RunEventRepository for NoopEvents {
        async fn insert(&self, _: &NewRunEvent) -> Result<i64, StorageError> {
            Ok(1)
        }
    }
    #[tokio::test]
    async fn preparation_ready_after_deadline_is_rejected() {
        let claimed = ClaimedPublishRecord {
            id: 1,
            idempotency_key: "test".into(),
            category_key: "ai".into(),
            report_date: "2026-09-10".into(),
            target_timezone: "UTC".into(),
            render_version: 1,
            selection_policy_version: 1,
            state: "stored_local".into(),
            remote_target: None,
            attempt_count: 1,
            rendered_at: None,
            last_error_kind: None,
            unverified_write_path: None,
            unverified_write_blob_sha: None,
        };
        let config = RenderConfig {
            category_display_name: "AI".into(),
            report_title: "Daily".into(),
            generated_at: OffsetDateTime::now_utc(),
            templates: Default::default(),
        };
        let deadline = Instant::now() + StdDuration::from_millis(20);
        let result = prepare_remote_report(
            &ReadyAfterDeadline(deadline),
            &claimed,
            &config,
            deadline,
            &RunEventEmitter {
                run_id: "test",
                stage: "publish",
                repo: &NoopEvents,
            },
            "remote",
        )
        .await;
        assert!(
            matches!(result, Err(RemotePreparationError::Timeout)),
            "a ready future must not escape an elapsed preparation budget"
        );
    }
}
