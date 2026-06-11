//! [`PublishFlow`] 的 store_local 阶段：claim → 渲染 → 落本地 → 推进/终态。

use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_report::RenderConfig;
use rss_ai_news_storage::{
    ClaimRequest, PublishAdvanceExtras, PublishState, PublishTimestampField,
    TerminalAdvanceOutcome, TerminalAdvanceStatus, build_owner_id, lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use super::PublishFlow;
use super::dto::{PublishStoreLocalOptions, PublishStoreLocalOutcome, PublishStoreLocalStatus};
use super::render_templates_from_ctx;
use crate::events::RunEventEmitter;

impl PublishFlow {
    pub async fn store_local(&self, opts: PublishStoreLocalOptions) -> PublishStoreLocalOutcome {
        self.store_local_record_inner(None, opts).await
    }

    pub async fn store_local_record(
        &self,
        publish_record_id: i64,
        opts: PublishStoreLocalOptions,
    ) -> PublishStoreLocalOutcome {
        self.store_local_record_inner(Some(publish_record_id), opts)
            .await
    }

    async fn store_local_record_inner(
        &self,
        publish_record_id: Option<i64>,
        opts: PublishStoreLocalOptions,
    ) -> PublishStoreLocalOutcome {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "publish",
            repo: self.ctx.event_repo.as_ref(),
        };

        // W15 §5：publish CLI 断点续跑时 store_local 可能是本次 run 的首次
        // claim（record 停在 rendered），同样接 ① + ②（codex W15-P4 复审）。
        self.run_publish_maintenance(&emitter).await;

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
                .claim_publish_by_ids(&claim, PublishState::Rendered, &[publish_record_id])
                .await
        } else {
            self.ctx
                .publish_record_repo
                .claim_rendered_for_local_store(&claim)
                .await
        };
        let claimed = match claimed_result {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::error!("claim rendered for local store failed: {error}");
                return PublishStoreLocalOutcome {
                    publish_record_id: 0,
                    status: PublishStoreLocalStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    local_path: None,
                    item_count: 0,
                };
            }
        };
        let claimed = match claimed.into_iter().next() {
            Some(claimed) => claimed,
            None => {
                return PublishStoreLocalOutcome {
                    publish_record_id: 0,
                    status: PublishStoreLocalStatus::NothingToClaim,
                    local_path: None,
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
                self.fail_claimed(claimed.id, &owner, &error, now, &emitter)
                    .await;
                return PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    local_path: None,
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
                        phase = "store_local.list_items",
                        ?persist_err,
                        "release_permanent_failure 持久化失败；保留上游错误向上抛（F15-fix4）"
                    );
                }
                return PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    local_path: None,
                    item_count: 0,
                };
            }
        };
        let item_count = items.len() as u32;

        let artifact = match self.ctx.publish_target_local.publish(&report).await {
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
                        phase = "store_local.target_publish",
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
                            "phase": "store_local",
                            "error_kind": error.error_kind(),
                            "budget_exhausted": budget_exhausted
                        })),
                    )
                    .await;
                return PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    local_path: None,
                    item_count,
                };
            }
        };

        let extras = PublishAdvanceExtras {
            local_path: artifact.local_path.clone(),
            remote_target: artifact.remote_target.clone(),
            commit_sha: artifact.commit_sha.clone(),
        };

        if claimed.remote_target.is_none() {
            let promote_article_ids = items.into_iter().map(|item| item.article_id).collect();
            match self
                .ctx
                .publish_record_repo
                .release_terminal_advance_with_articles(
                    claimed.id,
                    &owner,
                    PublishState::Rendered,
                    PublishState::PublishedLocal,
                    PublishTimestampField::LocalStoredAt,
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
                            "published locally",
                            Some(json!({
                                "phase": "store_local",
                                "mode": "local_only",
                                "item_count": item_count
                            })),
                        )
                        .await;
                    PublishStoreLocalOutcome {
                        publish_record_id: claimed.id,
                        status: PublishStoreLocalStatus::PublishedLocal,
                        local_path: artifact.local_path,
                        item_count,
                    }
                }
                Ok(TerminalAdvanceOutcome {
                    status: TerminalAdvanceStatus::PublishRecordConflict,
                }) => PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::Conflicted,
                    local_path: artifact.local_path,
                    item_count,
                },
                Ok(TerminalAdvanceOutcome {
                    status: TerminalAdvanceStatus::ArticleStateConflict { article_id },
                }) => PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::ArticleConflict { article_id },
                    local_path: artifact.local_path,
                    item_count,
                },
                Err(error) => PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    local_path: artifact.local_path,
                    item_count,
                },
            }
        } else {
            match self
                .ctx
                .publish_record_repo
                .release_advance(
                    claimed.id,
                    &owner,
                    PublishState::Rendered,
                    PublishState::StoredLocal,
                    PublishTimestampField::LocalStoredAt,
                    now,
                    extras,
                )
                .await
            {
                Ok(true) => PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::StoredLocal,
                    local_path: artifact.local_path,
                    item_count,
                },
                Ok(false) => PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::Conflicted,
                    local_path: artifact.local_path,
                    item_count,
                },
                Err(error) => PublishStoreLocalOutcome {
                    publish_record_id: claimed.id,
                    status: PublishStoreLocalStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                    local_path: artifact.local_path,
                    item_count,
                },
            }
        }
    }
}
