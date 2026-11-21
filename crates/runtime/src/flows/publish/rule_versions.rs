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
        None => {
            repo.active_rule_or_register("render", "default", "default render", "v1")
                .await?
        }
    };
    let selection_policy_version = repo
        .active_rule_or_register(
            "selection_policy",
            "default",
            "default selection policy",
            "v1",
        )
        .await?;
    Ok(PublishRuleVersions {
        render_version,
        selection_policy_version,
    })
}
