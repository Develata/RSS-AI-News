//! [`PublishFlow`] 的 remote 阶段：单条 publish_remote + 批量 publish_remote_batch。
//!
//! `PreparedRemote` 为本阶段私有的批量预备态。

use std::collections::HashMap;
use std::sync::Arc;

use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_publish::PublishError;
use rss_ai_news_report::{RenderConfig, ReportError};
use rss_ai_news_storage::{
    ClaimRequest, PublishAdvanceExtras, PublishState, PublishTimestampField,
    TerminalAdvanceOutcome, TerminalAdvanceStatus, build_owner_id, lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use super::PublishFlow;
use super::dto::{
    PublishRemoteBatchOptions, PublishRemoteBatchOutcome, PublishRemoteOptions,
    PublishRemoteOutcome, PublishRemoteStatus,
};
use super::render_templates_from_ctx;
use crate::events::RunEventEmitter;
use crate::flows::maintenance::emit_maintenance_outcome;

struct PreparedRemote {
    publish_record_id: i64,
    report: rss_ai_news_domain::dto::publish::RenderedReport,
    promote_article_ids: Vec<i64>,
    item_count: u32,
}

impl PublishFlow {
    pub async fn publish_remote(&self, opts: PublishRemoteOptions) -> PublishRemoteOutcome {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
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

        let now = OffsetDateTime::now_utc();
        let owner = build_owner_id();
        let claim = ClaimRequest {
            owner: owner.clone(),
            now,
            lease_expires_at: lease_expires_at(
                now,
                Duration::seconds(self.ctx.app.lease.publish_duration_seconds as i64),
            ),
            batch_size: 1,
            max_attempts: self.ctx.app.retry.publish_max_attempts,
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

        emitter
            .emit(
                "publish_started",
                "info",
                Some("publish_record"),
                Some(claimed.id),
                "remote publish started",
                Some(json!({ "phase": "publish_remote" })),
            )
            .await;

        let render_config = RenderConfig {
            category_display_name: opts.category_display_name,
            report_title: opts.report_title,
            generated_at: opts.generated_at,
            templates: render_templates_from_ctx(&self.ctx, opts.path_template.as_deref()),
        };
        let report = match rss_ai_news_report::rebuild_markdown(
            self.ctx.publish_record_repo.as_ref(),
            self.ctx.publish_item_repo.as_ref(),
            claimed.id,
            &render_config,
        )
        .await
        {
            Ok(report) => report,
            Err(error) => {
                self.release_report_error(claimed.id, &owner, &error, now, &emitter)
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

        let items = match self
            .ctx
            .publish_item_repo
            .list_by_publish_record(claimed.id)
            .await
        {
            Ok(items) => items,
            Err(error) => {
                if let Err(persist_err) = self
                    .ctx
                    .publish_record_repo
                    .release_permanent_failure(
                        claimed.id,
                        &owner,
                        &error.to_string(),
                        error.error_kind(),
                        now,
                    )
                    .await
                {
                    tracing::warn!(
                        publish_record_id = claimed.id,
                        phase = "publish_remote.list_items",
                        ?persist_err,
                        "release_permanent_failure 持久化失败；保留上游错误向上抛（F15-fix4）"
                    );
                }
                emitter
                    .emit(
                        "publish_failed",
                        "error",
                        Some("publish_record"),
                        Some(claimed.id),
                        &error.to_string(),
                        Some(json!({
                            "phase": "publish_remote",
                            "error_kind": error.error_kind()
                        })),
                    )
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
        let item_count = items.len() as u32;

        let artifact = match target.publish(&report).await {
            Ok(artifact) => artifact,
            Err(error) => {
                // W15 §3：retryable 路径在 release SQL 内按预算折叠（耗尽 → failed）。
                let mut budget_exhausted = false;
                let release_result = if error.is_retryable() {
                    self.ctx
                        .publish_record_repo
                        .release_retryable_failure(
                            claimed.id,
                            &owner,
                            &error.display_user(),
                            error.error_kind(),
                            self.ctx.app.retry.publish_max_attempts,
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
                            claimed.id,
                            &owner,
                            &error.display_user(),
                            error.error_kind(),
                            now,
                        )
                        .await
                };
                if let Err(persist_err) = release_result {
                    tracing::warn!(
                        publish_record_id = claimed.id,
                        phase = "publish_remote.target_publish",
                        retryable = error.is_retryable(),
                        ?persist_err,
                        "release_*_failure 持久化失败；保留上游错误向上抛（F15-fix4）"
                    );
                }
                emitter
                    .emit(
                        "publish_failed",
                        "error",
                        Some("publish_record"),
                        Some(claimed.id),
                        &error.display_user(),
                        Some(json!({
                            "phase": "publish_remote",
                            "error_kind": error.error_kind(),
                            "budget_exhausted": budget_exhausted
                        })),
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

        let promote_article_ids = items.into_iter().map(|item| item.article_id).collect();
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
            run_id: &self.ctx.run_id,
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
        let maintenance_now = OffsetDateTime::now_utc();
        let reclaimed = self
            .ctx
            .publish_record_repo
            .reclaim_expired_leases(maintenance_now)
            .await;
        let swept = self
            .ctx
            .publish_record_repo
            .terminalize_exhausted(self.ctx.app.retry.publish_max_attempts, maintenance_now)
            .await;
        emit_maintenance_outcome(&emitter, "publish_records", reclaimed, Some(swept)).await;

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
                Duration::seconds(self.ctx.app.lease.publish_duration_seconds as i64),
            ),
            batch_size: ids.len() as u32,
            max_attempts: self.ctx.app.retry.publish_max_attempts,
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

            emitter
                .emit(
                    "publish_started",
                    "info",
                    Some("publish_record"),
                    Some(claimed.id),
                    "remote batch publish item started",
                    Some(json!({ "phase": "publish_remote_batch" })),
                )
                .await;

            let render_config = RenderConfig {
                category_display_name: item.category_display_name,
                report_title: item.report_title,
                generated_at: item.generated_at,
                templates: render_templates_from_ctx(&self.ctx, item.path_template.as_deref()),
            };
            let report = match rss_ai_news_report::rebuild_markdown(
                self.ctx.publish_record_repo.as_ref(),
                self.ctx.publish_item_repo.as_ref(),
                claimed.id,
                &render_config,
            )
            .await
            {
                Ok(report) => report,
                Err(error) => {
                    self.release_report_error(claimed.id, &owner, &error, now, &emitter)
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

            let items = match self
                .ctx
                .publish_item_repo
                .list_by_publish_record(claimed.id)
                .await
            {
                Ok(items) => items,
                Err(error) => {
                    if let Err(persist_err) = self
                        .ctx
                        .publish_record_repo
                        .release_permanent_failure(
                            claimed.id,
                            &owner,
                            &error.to_string(),
                            error.error_kind(),
                            now,
                        )
                        .await
                    {
                        tracing::warn!(
                            publish_record_id = claimed.id,
                            phase = "publish_remote_batch.list_items",
                            ?persist_err,
                            "release_permanent_failure 持久化失败；保留上游错误向上抛（F15-fix4）"
                        );
                    }
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
            let item_count = items.len() as u32;
            prepared.push(PreparedRemote {
                publish_record_id: claimed.id,
                report,
                promote_article_ids: items.into_iter().map(|item| item.article_id).collect(),
                item_count,
            });
        }

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
        let batch = match target.publish_many(&reports).await {
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
                self.release_publish_error_for_prepared(&prepared, &owner, &error, now, &emitter)
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
                self.release_publish_error_for_prepared(&prepared, &owner, &error, now, &emitter)
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

        for (item, artifact) in prepared.into_iter().zip(batch.artifacts.into_iter()) {
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
        now: OffsetDateTime,
        emitter: &RunEventEmitter<'_>,
    ) {
        for item in prepared {
            // W15 §3：retryable 路径在 release SQL 内按预算折叠（耗尽 → failed）。
            let mut budget_exhausted = false;
            let release_result = if error.is_retryable() {
                self.ctx
                    .publish_record_repo
                    .release_retryable_failure(
                        item.publish_record_id,
                        owner,
                        &error.display_user(),
                        error.error_kind(),
                        self.ctx.app.retry.publish_max_attempts,
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
                        item.publish_record_id,
                        owner,
                        &error.display_user(),
                        error.error_kind(),
                        now,
                    )
                    .await
            };
            if let Err(persist_err) = release_result {
                tracing::warn!(
                    publish_record_id = item.publish_record_id,
                    phase = "publish_remote_batch.target_publish",
                    retryable = error.is_retryable(),
                    ?persist_err,
                    "release_*_failure 持久化失败；保留上游错误向上抛（F15-fix4）"
                );
            }
            emitter
                .emit(
                    "publish_failed",
                    "error",
                    Some("publish_record"),
                    Some(item.publish_record_id),
                    &error.display_user(),
                    Some(json!({
                        "phase": "publish_remote_batch",
                        "error_kind": error.error_kind(),
                        "budget_exhausted": budget_exhausted
                    })),
                )
                .await;
        }
    }

    async fn release_report_error(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &ReportError,
        now: OffsetDateTime,
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
                now,
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
