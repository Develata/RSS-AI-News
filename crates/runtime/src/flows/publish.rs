use std::sync::Arc;

use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::publish::PublishRequest;
use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_report::{
    RenderConfig, ReportError, SelectionConfig, SnapshotConfig, freeze as snapshot_freeze,
    load_candidates, to_storage_items,
};
use rss_ai_news_storage::{
    ClaimRequest, FreezeSnapshotOutcome, FreezeSnapshotStatus, NewPublishRecord,
    PublishAdvanceExtras, PublishState, PublishTimestampField, TerminalAdvanceOutcome,
    TerminalAdvanceStatus, build_owner_id, lease_expires_at,
};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use crate::context::RunContext;
use crate::events::RunEventEmitter;

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
    pub max_items: u32,
    pub min_importance_score: Score0To100,
    pub include_unscored: bool,
    pub ai_enabled: bool,
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
}

#[derive(Debug, Clone)]
pub struct PublishStoreLocalOptions {
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct PublishRemoteOptions {
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at: OffsetDateTime,
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

pub struct PublishFlow {
    ctx: Arc<RunContext>,
}

impl PublishFlow {
    pub fn new(ctx: Arc<RunContext>) -> Self {
        Self { ctx }
    }

    pub fn build_idempotency_key(
        category_key: &str,
        report_date: &str,
        render_version: i64,
    ) -> String {
        format!("{category_key}-{report_date}-v{render_version}")
    }

    pub async fn init(
        &self,
        opts: PublishInitOptions,
    ) -> Result<PublishInitOutcome, crate::RuntimeError> {
        let key =
            Self::build_idempotency_key(&opts.category_key, &opts.report_date, opts.render_version);
        let new_record = NewPublishRecord {
            idempotency_key: key.clone(),
            category_key: opts.category_key,
            report_date: opts.report_date,
            target_timezone: opts.target_timezone,
            render_version: opts.render_version,
            selection_policy_version: opts.selection_policy_version,
            remote_target: opts.remote_target,
        };
        match self
            .ctx
            .publish_record_repo
            .create_if_new(&new_record)
            .await?
        {
            Some(id) => Ok(PublishInitOutcome::Created {
                publish_record_id: id,
            }),
            None => {
                let existing = self
                    .ctx
                    .publish_record_repo
                    .find_by_idempotency_key(&key)
                    .await?
                    .ok_or_else(|| {
                        crate::RuntimeError::Config(format!(
                            "idempotency_key {key} disappeared after conflict"
                        ))
                    })?;
                Ok(PublishInitOutcome::AlreadyExists {
                    publish_record_id: existing.id,
                    state: existing.state,
                })
            }
        }
    }

    pub async fn freeze(&self, opts: PublishFreezeOptions) -> PublishFreezeOutcome {
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
            .claim_pending_for_freeze(&claim)
            .await
        {
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
                let _ = self
                    .ctx
                    .publish_record_repo
                    .release_permanent_failure(
                        claimed.id,
                        &owner,
                        &error.to_string(),
                        error.error_kind(),
                        now,
                    )
                    .await;
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

    async fn fail_claimed(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &ReportError,
        now: OffsetDateTime,
        emitter: &RunEventEmitter<'_>,
    ) {
        let _ = self
            .ctx
            .publish_record_repo
            .release_permanent_failure(
                publish_record_id,
                owner,
                &error.display_user(),
                error.error_kind(),
                now,
            )
            .await;
        emitter
            .emit(
                "publish_failed",
                if matches!(error, ReportError::SnapshotEmpty) {
                    "warn"
                } else {
                    "error"
                },
                Some("publish_record"),
                Some(publish_record_id),
                &error.display_user(),
                Some(json!({ "phase": "freeze", "error_kind": error.error_kind() })),
            )
            .await;
    }

    pub async fn render(&self, opts: PublishRenderOptions) -> PublishRenderOutcome {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "publish",
            repo: self.ctx.event_repo.as_ref(),
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
            .claim_frozen_for_render(&claim)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::error!("claim frozen for render failed: {error}");
                return PublishRenderOutcome {
                    publish_record_id: 0,
                    status: PublishRenderStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                };
            }
        };
        let claimed = match claimed.into_iter().next() {
            Some(claimed) => claimed,
            None => {
                return PublishRenderOutcome {
                    publish_record_id: 0,
                    status: PublishRenderStatus::NothingToClaim,
                };
            }
        };

        let render_config = RenderConfig {
            category_display_name: opts.category_display_name,
            report_title: opts.report_title,
            generated_at: opts.generated_at,
        };
        if let Err(error) = rss_ai_news_report::rebuild_markdown(
            self.ctx.publish_record_repo.as_ref(),
            self.ctx.publish_item_repo.as_ref(),
            claimed.id,
            &render_config,
        )
        .await
        {
            self.fail_claimed(claimed.id, &owner, &error, now, &emitter)
                .await;
            return PublishRenderOutcome {
                publish_record_id: claimed.id,
                status: PublishRenderStatus::Failed {
                    error_kind: error.error_kind().to_string(),
                },
            };
        }

        match self
            .ctx
            .publish_record_repo
            .release_advance(
                claimed.id,
                &owner,
                PublishState::SnapshotFrozen,
                PublishState::Rendered,
                PublishTimestampField::RenderedAt,
                now,
                PublishAdvanceExtras::default(),
            )
            .await
        {
            Ok(true) => PublishRenderOutcome {
                publish_record_id: claimed.id,
                status: PublishRenderStatus::Rendered,
            },
            Ok(false) => PublishRenderOutcome {
                publish_record_id: claimed.id,
                status: PublishRenderStatus::Conflicted,
            },
            Err(error) => {
                tracing::error!("release_advance to rendered failed: {error}");
                PublishRenderOutcome {
                    publish_record_id: claimed.id,
                    status: PublishRenderStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                }
            }
        }
    }

    pub async fn store_local(&self, opts: PublishStoreLocalOptions) -> PublishStoreLocalOutcome {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "publish",
            repo: self.ctx.event_repo.as_ref(),
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
            .claim_rendered_for_local_store(&claim)
            .await
        {
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
                let _ = self
                    .ctx
                    .publish_record_repo
                    .release_permanent_failure(
                        claimed.id,
                        &owner,
                        &error.to_string(),
                        error.error_kind(),
                        now,
                    )
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
        let item_count = items.len() as u32;

        let artifact = match self.ctx.publish_target_local.publish(&report).await {
            Ok(artifact) => artifact,
            Err(error) => {
                if error.is_retryable() {
                    let _ = self
                        .ctx
                        .publish_record_repo
                        .release_retryable_failure(
                            claimed.id,
                            &owner,
                            &error.display_user(),
                            error.error_kind(),
                            now,
                        )
                        .await;
                } else {
                    let _ = self
                        .ctx
                        .publish_record_repo
                        .release_permanent_failure(
                            claimed.id,
                            &owner,
                            &error.display_user(),
                            error.error_kind(),
                            now,
                        )
                        .await;
                }
                emitter
                    .emit(
                        "publish_failed",
                        "error",
                        Some("publish_record"),
                        Some(claimed.id),
                        &error.display_user(),
                        Some(json!({ "phase": "store_local", "error_kind": error.error_kind() })),
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
                let _ = self
                    .ctx
                    .publish_record_repo
                    .release_permanent_failure(
                        claimed.id,
                        &owner,
                        &error.to_string(),
                        error.error_kind(),
                        now,
                    )
                    .await;
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
                if error.is_retryable() {
                    let _ = self
                        .ctx
                        .publish_record_repo
                        .release_retryable_failure(
                            claimed.id,
                            &owner,
                            &error.display_user(),
                            error.error_kind(),
                            now,
                        )
                        .await;
                } else {
                    let _ = self
                        .ctx
                        .publish_record_repo
                        .release_permanent_failure(
                            claimed.id,
                            &owner,
                            &error.display_user(),
                            error.error_kind(),
                            now,
                        )
                        .await;
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

    async fn release_report_error(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &ReportError,
        now: OffsetDateTime,
        emitter: &RunEventEmitter<'_>,
    ) {
        let _ = self
            .ctx
            .publish_record_repo
            .release_permanent_failure(
                publish_record_id,
                owner,
                &error.display_user(),
                error.error_kind(),
                now,
            )
            .await;
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
