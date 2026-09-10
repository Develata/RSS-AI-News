//! [`PublishFlow`] 的 remote 阶段：单条 publish_remote + 批量 publish_remote_batch。
//!
//! `PreparedRemote` 为本阶段私有的批量预备态。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_publish::PublishError;
use rss_ai_news_report::{RenderConfig, ReportError, load_frozen_items, render_markdown};
use rss_ai_news_storage::{
    ClaimRequest, ClaimedPublishRecord, PublishAdvanceExtras, PublishItemRepository, PublishState,
    PublishTimestampField, TerminalAdvanceOutcome, TerminalAdvanceStatus, build_owner_id,
    lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::time::Instant;

use super::PublishFlow;
use super::dto::{
    PublishRemoteBatchOptions, PublishRemoteBatchOutcome, PublishRemoteOptions,
    PublishRemoteOutcome, PublishRemoteStatus,
};
use super::render_templates_from_ctx;
use crate::events::RunEventEmitter;

struct PreparedRemote {
    publish_record_id: i64,
    promote_article_ids: Vec<i64>,
    item_count: u32,
}

// Start the shared preparation/remote budget before claiming so claim latency
// consumes it too. Reserve 20% for releases; claim and cleanup database writes
// retain their own failure/recovery semantics rather than being cancelled here.
fn remote_deadline(lease_seconds: u64) -> Instant {
    let lease = StdDuration::from_secs(lease_seconds);
    Instant::now() + (lease - lease / 5)
}

enum RemotePreparationError {
    Report(ReportError),
    Timeout,
}

impl RemotePreparationError {
    fn error_kind(&self) -> &str {
        match self {
            Self::Report(error) => error.error_kind(),
            Self::Timeout => "remote_timeout",
        }
    }
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

impl PublishFlow {
    pub async fn publish_remote(&self, opts: PublishRemoteOptions) -> PublishRemoteOutcome {
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
        let claimed = match self
            .ctx
            .publish_record_repo
            .claim_local_for_remote_publish(&claim)
            .await
        {
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

        let render_config = RenderConfig {
            category_display_name: opts.category_display_name,
            report_title: opts.report_title,
            generated_at: opts.generated_at,
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
                self.release_preparation_error(claimed.id, &owner, &error, &emitter)
                    .await;
                return PublishRemoteOutcome {
                    publish_record_id: claimed.id,
                    status: PublishRemoteStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    commit_sha: None,
                    remote_target: None,
                    item_count: 0,
                };
            }
        };

        let item_count = promote_article_ids.len() as u32;

        let result = if Instant::now() >= deadline {
            Err(PublishError::RemoteTimeout)
        } else {
            tokio::time::timeout_at(deadline, target.publish(&report))
                .await
                .unwrap_or(Err(PublishError::RemoteTimeout))
        };
        let now = OffsetDateTime::now_utc();
        let artifact = match result {
            Ok(artifact) => artifact,
            Err(error) => {
                self.release_publish_error_for_record(
                    claimed.id,
                    &owner,
                    &error,
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

        let extras = PublishAdvanceExtras {
            local_path: None,
            remote_target: artifact.remote_target.clone(),
            commit_sha: artifact.commit_sha.clone(),
        };
        match self
            .ctx
            .publish_record_repo
            .release_terminal_advance_with_articles(
                claimed.id,
                &owner,
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
                        Some(claimed.id),
                        "published remotely",
                        Some(json!({
                            "phase": "publish_remote",
                            "commit_sha": artifact.commit_sha.as_deref(),
                            "remote_target": artifact.remote_target.as_deref(),
                            "item_count": item_count
                        })),
                    )
                    .await;
                PublishRemoteOutcome {
                    publish_record_id: claimed.id,
                    status: PublishRemoteStatus::PublishedRemote,
                    commit_sha: artifact.commit_sha,
                    remote_target: artifact.remote_target,
                    item_count,
                }
            }
            Ok(TerminalAdvanceOutcome {
                status: TerminalAdvanceStatus::PublishRecordConflict,
            }) => PublishRemoteOutcome {
                publish_record_id: claimed.id,
                status: PublishRemoteStatus::Conflicted,
                commit_sha: artifact.commit_sha,
                remote_target: artifact.remote_target,
                item_count,
            },
            Ok(TerminalAdvanceOutcome {
                status: TerminalAdvanceStatus::ArticleStateConflict { article_id },
            }) => PublishRemoteOutcome {
                publish_record_id: claimed.id,
                status: PublishRemoteStatus::ArticleConflict { article_id },
                commit_sha: artifact.commit_sha,
                remote_target: artifact.remote_target,
                item_count,
            },
            Err(error) => PublishRemoteOutcome {
                publish_record_id: claimed.id,
                status: PublishRemoteStatus::Failed {
                    error_kind: error.error_kind().to_string(),
                },
                commit_sha: artifact.commit_sha,
                remote_target: artifact.remote_target,
                item_count,
            },
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
        let mut reports = Vec::new();
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

            let render_config = RenderConfig {
                category_display_name: item.category_display_name,
                report_title: item.report_title,
                generated_at: item.generated_at,
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
                    self.release_preparation_error(claimed.id, &owner, &error, &emitter)
                        .await;
                    outcomes.push(PublishRemoteOutcome {
                        publish_record_id: claimed.id,
                        status: PublishRemoteStatus::Failed {
                            error_kind: error.error_kind().to_string(),
                        },
                        commit_sha: None,
                        remote_target: None,
                        item_count: 0,
                    });
                    continue;
                }
            };

            let item_count = promote_article_ids.len() as u32;
            reports.push(report);
            prepared.push(PreparedRemote {
                publish_record_id: claimed.id,
                promote_article_ids,
                item_count,
            });
        }

        if prepared.is_empty() {
            return PublishRemoteBatchOutcome {
                items: outcomes,
                commit_sha: None,
            };
        }

        let result = if Instant::now() >= deadline {
            Err(PublishError::RemoteTimeout)
        } else {
            tokio::time::timeout_at(deadline, target.publish_many(&reports))
                .await
                .unwrap_or(Err(PublishError::RemoteTimeout))
        };
        let now = OffsetDateTime::now_utc();
        let batch = match result {
            Ok(batch) if batch.artifacts.len() == prepared.len() => batch,
            Ok(batch) => {
                let error = PublishError::GitHubApiError {
                    status: 502,
                    message: format!(
                        "publish_many returned {} artifacts for {} reports",
                        batch.artifacts.len(),
                        prepared.len()
                    ),
                };
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
            let extras = PublishAdvanceExtras {
                local_path: None,
                remote_target: artifact.remote_target.clone(),
                commit_sha: artifact.commit_sha.clone(),
            };
            let status = match self
                .ctx
                .publish_record_repo
                .release_terminal_advance_with_articles(
                    item.publish_record_id,
                    &owner,
                    PublishState::StoredLocal,
                    PublishState::PublishedRemote,
                    PublishTimestampField::RemotePublishedAt,
                    item.promote_article_ids,
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
                            Some(item.publish_record_id),
                            "published remotely in batch",
                            Some(json!({
                                "phase": "publish_remote_batch",
                                "commit_sha": artifact.commit_sha.as_deref(),
                                "remote_target": artifact.remote_target.as_deref(),
                                "item_count": item.item_count
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
                Err(error) => PublishRemoteStatus::Failed {
                    error_kind: error.error_kind().to_string(),
                },
            };
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
                "publish_remote_batch",
                emitter,
            )
            .await;
        }
    }

    async fn release_publish_error_for_record(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &PublishError,
        phase: &'static str,
        emitter: &RunEventEmitter<'_>,
    ) {
        let now = OffsetDateTime::now_utc();
        let message = error.display_user();
        let mut budget_exhausted = false;
        let release_result = if error.is_retryable() {
            self.ctx
                .publish_record_repo
                .release_retryable_failure(
                    publish_record_id,
                    owner,
                    &message,
                    error.error_kind(),
                    self.ctx.retry.publish_max_attempts,
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
                })),
            )
            .await;
    }

    async fn release_preparation_error(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &RemotePreparationError,
        emitter: &RunEventEmitter<'_>,
    ) {
        match error {
            RemotePreparationError::Report(error) => {
                self.release_report_error(publish_record_id, owner, error, emitter)
                    .await;
            }
            RemotePreparationError::Timeout => {
                self.release_publish_error_for_record(
                    publish_record_id,
                    owner,
                    &PublishError::RemoteTimeout,
                    "publish_remote.prepare",
                    emitter,
                )
                .await;
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
        if let Err(persist_err) = self
            .ctx
            .publish_record_repo
            .release_permanent_failure(
                publish_record_id,
                owner,
                &error.display_user(),
                error.error_kind(),
                OffsetDateTime::now_utc(),
            )
            .await
        {
            tracing::warn!(
                publish_record_id,
                phase = "publish_remote.report_error",
                ?persist_err,
                "release_permanent_failure 持久化失败；保留上游 ReportError 向上抛（F15-fix4）"
            );
        }
        emitter
            .emit(
                "publish_failed",
                "error",
                Some("publish_record"),
                Some(publish_record_id),
                &error.display_user(),
                Some(json!({
                    "phase": "publish_remote",
                    "error_kind": error.error_kind()
                })),
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
