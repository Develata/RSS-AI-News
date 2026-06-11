//! [`PublishFlow`] 的 freeze 阶段：select 候选 → 冻结快照 → 写 publish_items。
//!
//! `fail_claimed` / 共享 helper 在 [`super`]（父模块私有项，子模块可见）。

use rss_ai_news_domain::dto::publish::PublishRequest;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_report::{
    ReportError, SelectionConfig, SnapshotConfig, freeze as snapshot_freeze, load_candidates,
    to_storage_items,
};
use rss_ai_news_storage::{
    ClaimRequest, FreezeSnapshotOutcome, FreezeSnapshotStatus, PublishState, build_owner_id,
    lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use super::PublishFlow;
use super::dto::{PublishFreezeOptions, PublishFreezeOutcome, PublishFreezeStatus};
use crate::events::RunEventEmitter;
use crate::flows::maintenance::emit_maintenance_outcome;

impl PublishFlow {
    pub async fn freeze(&self, opts: PublishFreezeOptions) -> PublishFreezeOutcome {
        self.freeze_record_inner(None, opts).await
    }

    pub async fn freeze_record(
        &self,
        publish_record_id: i64,
        opts: PublishFreezeOptions,
    ) -> PublishFreezeOutcome {
        self.freeze_record_inner(Some(publish_record_id), opts)
            .await
    }

    async fn freeze_record_inner(
        &self,
        publish_record_id: Option<i64>,
        opts: PublishFreezeOptions,
    ) -> PublishFreezeOutcome {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "publish",
            repo: self.ctx.event_repo.as_ref(),
        };
        emitter
            .emit(
                "publish_started",
                "info",
                None,
                None,
                "publish freeze run started",
                Some(json!({ "phase": "freeze", "category": opts.category_key })),
            )
            .await;

        // W15 §5：首次 claim 前执行一次 ① reclaim + ② sweep（顺序固定，best-effort）。
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
        let claimed_result = if let Some(publish_record_id) = publish_record_id {
            self.ctx
                .publish_record_repo
                .claim_publish_by_ids(&claim, PublishState::Pending, &[publish_record_id])
                .await
        } else {
            self.ctx
                .publish_record_repo
                .claim_pending_for_freeze(&claim)
                .await
        };
        let claimed = match claimed_result {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::error!("claim publish records failed: {error}");
                return PublishFreezeOutcome {
                    publish_record_id: 0,
                    status: PublishFreezeStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    item_count: 0,
                };
            }
        };
        let claimed = match claimed.into_iter().next() {
            Some(claimed) => claimed,
            None => {
                emitter
                    .emit(
                        "publish_completed",
                        "info",
                        None,
                        None,
                        "no publish records to freeze",
                        Some(json!({ "phase": "freeze", "claimed": 0 })),
                    )
                    .await;
                return PublishFreezeOutcome {
                    publish_record_id: 0,
                    status: PublishFreezeStatus::NothingToClaim,
                    item_count: 0,
                };
            }
        };

        let request = PublishRequest {
            category_key: opts.category_key.clone(),
            report_date: claimed.report_date.clone(),
            target_timezone: claimed.target_timezone.clone(),
            render_version_id: claimed.render_version,
            selection_policy_version_id: claimed.selection_policy_version,
            published_since: published_since(now, opts.candidate_window_hours),
            published_until: now,
            max_items: opts.max_items,
            min_importance_score: opts.min_importance_score,
            include_unscored: opts.include_unscored,
        };
        let candidates = match load_candidates(
            self.ctx.publish_item_repo.as_ref(),
            &request,
            &SelectionConfig {
                ai_enabled: opts.ai_enabled,
            },
        )
        .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                self.fail_claimed(claimed.id, &owner, &error, now, &emitter)
                    .await;
                return PublishFreezeOutcome {
                    publish_record_id: claimed.id,
                    status: PublishFreezeStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    item_count: 0,
                };
            }
        };

        if candidates.is_empty() {
            let error = ReportError::SnapshotEmpty;
            self.fail_claimed(claimed.id, &owner, &error, now, &emitter)
                .await;
            return PublishFreezeOutcome {
                publish_record_id: claimed.id,
                status: PublishFreezeStatus::SnapshotEmpty,
                item_count: 0,
            };
        }

        let promote_ids = candidates
            .iter()
            .filter(|candidate| candidate.article_ai_result_id.is_none())
            .map(|candidate| candidate.article_id)
            .collect::<Vec<_>>();

        let frozen = match snapshot_freeze(
            candidates,
            &SnapshotConfig {
                excerpt_max_chars: opts.excerpt_max_chars,
            },
        ) {
            Ok(frozen) => frozen,
            Err(error) => {
                self.fail_claimed(claimed.id, &owner, &error, now, &emitter)
                    .await;
                return PublishFreezeOutcome {
                    publish_record_id: claimed.id,
                    status: PublishFreezeStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    item_count: 0,
                };
            }
        };

        let item_count = frozen.len() as u32;
        let storage_items = to_storage_items(&frozen);
        let result = self
            .ctx
            .publish_item_repo
            .freeze_snapshot(
                claimed.id,
                &owner,
                storage_items,
                promote_ids,
                OffsetDateTime::now_utc(),
            )
            .await;

        match result {
            Ok(FreezeSnapshotOutcome {
                status: FreezeSnapshotStatus::Frozen,
                item_ids,
            }) => {
                emitter
                    .emit(
                        "publish_succeeded",
                        "info",
                        Some("publish_record"),
                        Some(claimed.id),
                        "snapshot frozen",
                        Some(json!({ "phase": "freeze", "item_count": item_ids.len() })),
                    )
                    .await;
                PublishFreezeOutcome {
                    publish_record_id: claimed.id,
                    status: PublishFreezeStatus::Frozen,
                    item_count,
                }
            }
            Ok(FreezeSnapshotOutcome {
                status: FreezeSnapshotStatus::PublishRecordConflict,
                ..
            }) => PublishFreezeOutcome {
                publish_record_id: claimed.id,
                status: PublishFreezeStatus::Conflicted,
                item_count: 0,
            },
            Ok(FreezeSnapshotOutcome {
                status: FreezeSnapshotStatus::ArticleStateConflict { article_id },
                ..
            }) => PublishFreezeOutcome {
                publish_record_id: claimed.id,
                status: PublishFreezeStatus::ArticleConflict { article_id },
                item_count: 0,
            },
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
                        phase = "freeze",
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
                        Some(json!({ "phase": "freeze", "error_kind": error.error_kind() })),
                    )
                    .await;
                PublishFreezeOutcome {
                    publish_record_id: claimed.id,
                    status: PublishFreezeStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    item_count: 0,
                }
            }
        }
    }
}

fn published_since(now: OffsetDateTime, window_hours: u32) -> OffsetDateTime {
    if window_hours == 0 {
        OffsetDateTime::UNIX_EPOCH
    } else {
        now - Duration::hours(i64::from(window_hours))
    }
}
