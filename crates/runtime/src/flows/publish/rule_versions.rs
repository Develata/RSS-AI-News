//! Rule-version bootstrap for a publish run (`publish` / `publish-all`).
//!
//! The CLI shell must not write the database directly (plan/09 §6); the
//! render / selection_policy registration therefore lives here, shared by both
//! entry points. `rebuild-report` deliberately does not use it: it only reads
//! the active render rule and never seeds one.

use rss_ai_news_storage::RuleVersionRepository;

use crate::error::RuntimeError;

/// Rule versions a publish run stamps on its records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishRuleVersions {
    pub render_version: i64,
    pub selection_policy_version: i64,
}

/// `--force`: record a fresh render trace row for this invocation.
#[derive(Debug, Clone, Copy)]
pub struct ForceRenderTrace<'a> {
    /// Category key for `publish`, `"all"` for `publish-all`.
    pub scope: &'a str,
    pub report_date: &'a str,
    /// Unique per invocation, so two forces in the same second never share
    /// (and silently reuse) one trace row.
    pub run_id: &'a str,
}

/// Resolves (seeding on first use) the active `render` and `selection_policy`
/// rules. With `force`, the render version is instead a new `pending` trace row
/// — never `active`, so it cannot become the default render rule even on a
/// fresh database (ADR 0004 partial unique stays with the default row).
pub async fn bootstrap_publish_rule_versions(
    repo: &dyn RuleVersionRepository,
    force: Option<ForceRenderTrace<'_>>,
) -> Result<PublishRuleVersions, RuntimeError> {
    let render_version = match force {
        Some(trace) => {
            let tag = format!(
                "force-{}-{}-{}",
                trace.scope, trace.report_date, trace.run_id
            );
            repo.insert_pending_rule("render", &tag, "force render trace", "v1")
                .await?
        }
        None => active_rule_id(repo, "render", "default render").await?,
    };
    let selection_policy_version =
        active_rule_id(repo, "selection_policy", "default selection policy").await?;
    Ok(PublishRuleVersions {
        render_version,
        selection_policy_version,
    })
}

/// The active `kind` rule, seeding `default` when the kind has no row at all.
/// `active_rule_or_register` reads an existing `default` row back by tag on a
/// conflict, whatever its status; when that row was superseded and nothing is
/// active, it would hand out a superseded id (R1-P-S6). The returned id is
/// therefore checked against the active row and refused otherwise, before any
/// record is created with it.
async fn active_rule_id(
    repo: &dyn RuleVersionRepository,
    kind: &str,
    description: &str,
) -> Result<i64, RuntimeError> {
    let id = repo
        .active_rule_or_register(kind, "default", description, "v1")
        .await?;
    match repo.active_rule(kind).await? {
        Some(active) if active.id == id => Ok(id),
        Some(active) => Err(RuntimeError::Config(format!(
            "{kind} rule version {id} is not the active one (active is {}); retry the publish",
            active.id
        ))),
        None => Err(RuntimeError::Config(format!(
            "no active {kind} rule version: the registered 'default' row {id} is not active; \
             activate a {kind} rule before publishing"
        ))),
    }
}
