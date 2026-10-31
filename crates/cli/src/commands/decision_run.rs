//! `decision-run`: the typed-decision shadow runner (ADR 0011). Not part of
//! `run`; schedule it separately. Every configuration check (section,
//! credentials, questions, lease bound, first-round budget) happens before
//! storage is opened, so a misconfigured run exits 78 without a connection.

use std::{
    io::{self, Write},
    sync::Arc,
    time::Instant,
};

use rss_ai_news_config::{self as config, LoadedConfig};
use rss_ai_news_decision::{
    DecisionSettings, Provider, SystemOneClient, canonical_policy, policy_version,
    provider::validate_questions, questions_from_section,
};
use rss_ai_news_runtime::{
    CategoryPlan, CategoryStats, DecisionRunFlow, DecisionRunOptions, DecisionRunSummary,
    RuntimeError,
};
use rss_ai_news_storage::{DecisionExecution, DecisionIdentity};
use serde::Serialize;

use crate::{
    args::{Cli, DecisionRunArgs},
    context_factory::{build_decision_run_deps, open_write_storage},
    error::CliError,
    exit_code::ExitCode,
    output::{CommandSummary, RenderedError},
};

#[derive(Debug, Clone, Serialize)]
pub struct DecisionCategorySummary {
    pub category: String,
    pub generated: u64,
    pub claimed: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub retried: u64,
    pub refunded: u64,
    pub other_identity_pending: u64,
    pub paused: Option<&'static str>,
}

impl From<&CategoryStats> for DecisionCategorySummary {
    fn from(stats: &CategoryStats) -> Self {
        Self {
            category: stats.category.clone(),
            generated: stats.generated,
            claimed: stats.claimed,
            succeeded: stats.succeeded,
            failed: stats.failed,
            retried: stats.retried,
            refunded: stats.refunded,
            other_identity_pending: stats.other_identity_pending,
            paused: stats.paused,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionRunCommandSummary {
    pub execution_key: String,
    pub categories: Vec<DecisionCategorySummary>,
    pub claimed: u64,
    /// Provider calls, i.e. spent request budget.
    pub calls: u64,
    pub max_requests: u32,
    pub input_tokens: u64,
    pub budget_exhausted: bool,
    pub rate_limited: bool,
    /// `kind` of the stop, if the run stopped early.
    pub aborted: Option<String>,
    pub storage_errors: u64,
    pub storage_timeouts: u64,
    pub release_failed: u64,
    pub owner_mismatch: u64,
    pub tasks_panicked: u64,
    pub drain_timeout: bool,
    pub raw_purged: u64,
    pub raw_gc_backlog: bool,
    pub duration_seconds: f64,
    #[serde(skip)]
    exit_class: u8,
}

impl DecisionRunCommandSummary {
    fn new(
        summary: &DecisionRunSummary,
        execution_key: String,
        max_requests: u32,
        started: Instant,
    ) -> Self {
        Self {
            execution_key,
            categories: summary.categories.iter().map(Into::into).collect(),
            claimed: summary.claimed,
            calls: summary.calls,
            max_requests,
            input_tokens: summary.input_tokens,
            budget_exhausted: summary.budget_exhausted,
            rate_limited: summary.rate_limited,
            aborted: summary
                .aborted
                .as_ref()
                .map(|abort| abort.kind().to_string()),
            storage_errors: summary.storage_errors,
            storage_timeouts: summary.storage_timeouts,
            release_failed: summary.release_failed,
            owner_mismatch: summary.owner_mismatch,
            tasks_panicked: summary.tasks_panicked,
            drain_timeout: summary.drain_timeout,
            raw_purged: summary.raw_purged,
            raw_gc_backlog: summary.raw_gc_backlog,
            duration_seconds: started.elapsed().as_secs_f64(),
            exit_class: summary.exit_class(),
        }
    }
}

impl CommandSummary for DecisionRunCommandSummary {
    fn exit_code(&self) -> ExitCode {
        ExitCode::from_i32(i32::from(self.exit_class))
    }

    fn errors(&self) -> Vec<RenderedError> {
        match (&self.aborted, self.exit_class) {
            (Some(kind), 1 | 78) => vec![RenderedError {
                kind: format!("decision_run_{kind}"),
                message: format!("decision-run stopped early ({kind})"),
            }],
            _ => Vec::new(),
        }
    }

    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "decision-run {}", self.execution_key)?;
        for category in &self.categories {
            writeln!(
                writer,
                "  {}: generated {}, claimed {}, succeeded {}, failed {}, retried {}, refunded {}{}{}",
                category.category,
                category.generated,
                category.claimed,
                category.succeeded,
                category.failed,
                category.retried,
                category.refunded,
                if category.other_identity_pending > 0 {
                    format!(
                        ", {} pending for another policy or provider",
                        category.other_identity_pending
                    )
                } else {
                    String::new()
                },
                category
                    .paused
                    .map(|reason| format!(" (paused: {reason})"))
                    .unwrap_or_default(),
            )?;
        }
        writeln!(
            writer,
            "  calls {} / {}, input tokens {}{}{}",
            self.calls,
            self.max_requests,
            self.input_tokens,
            if self.budget_exhausted {
                ", budget exhausted"
            } else {
                ""
            },
            if self.rate_limited {
                ", rate limited"
            } else {
                ""
            },
        )?;
        if let Some(kind) = &self.aborted {
            writeln!(writer, "  stopped early: {kind}")?;
        }
        if self.raw_purged > 0 || self.raw_gc_backlog {
            writeln!(
                writer,
                "  raw responses expired: {}{}",
                self.raw_purged,
                if self.raw_gc_backlog {
                    " (cap reached)"
                } else {
                    ""
                }
            )?;
        }
        writeln!(writer, "  duration {:.1}s", self.duration_seconds)
    }
}

fn config_error(source: &str, field: &str, message: impl Into<String>) -> CliError {
    CliError::Config(config::ConfigError::ValidationFailed {
        report: config::DiagnosticReport::new(vec![config::Diagnostic::new(
            source.to_string(),
            field.to_string(),
            message.into(),
        )]),
    })
}

fn issues_error(source: &str, issues: Vec<rss_ai_news_decision::Issue>) -> CliError {
    CliError::Config(config::ConfigError::ValidationFailed {
        report: config::DiagnosticReport::new(
            issues
                .into_iter()
                .map(|issue| {
                    config::Diagnostic::new(source.to_string(), issue.field, issue.message)
                })
                .collect(),
        ),
    })
}

fn settings(loaded: &LoadedConfig) -> Result<DecisionSettings, CliError> {
    let section = loaded.decision.app.as_ref().ok_or_else(|| {
        config_error(
            "app.toml",
            "decision",
            "decision-run needs a [decision] section with enabled = true",
        )
    })?;
    let (settings, _warnings) = DecisionSettings::from_section(section)
        .map_err(|issues| issues_error("app.toml", issues))?;
    if !settings.enabled {
        return Err(config_error(
            "app.toml",
            "decision.enabled",
            "decision-run needs enabled = true",
        ));
    }
    Ok(settings)
}

/// The selected categories that ask questions, in configuration order.
fn plans(
    loaded: &LoadedConfig,
    settings: &DecisionSettings,
    execution_key: &str,
) -> Result<Vec<CategoryPlan>, CliError> {
    let selected: Vec<_> = loaded.categories_filtered().collect();
    if selected.is_empty() {
        return Err(CliError::Runtime(RuntimeError::Config(
            "no category selected".to_string(),
        )));
    }
    let mut plans = Vec::new();
    for category in selected {
        let key = &category.category.key;
        let Some(section) = loaded
            .decision
            .categories
            .iter()
            .find(|section| &section.category_key == key)
        else {
            continue;
        };
        let questions = questions_from_section(&section.value)
            .map_err(|issues| issues_error(&section.source, issues))?;
        let issues =
            validate_questions(settings.provider, &questions, "category.decision.questions");
        if !issues.is_empty() {
            return Err(issues_error(&section.source, issues));
        }
        if questions.is_empty() {
            continue;
        }
        plans.push(CategoryPlan {
            identity: DecisionIdentity {
                category_key: key.clone(),
                policy_version: policy_version(&questions, settings.max_state_chars),
                execution_key: execution_key.to_string(),
            },
            canonical_policy_json: canonical_policy(&questions, settings.max_state_chars),
            questions,
        });
    }
    if plans.is_empty() {
        return Err(config_error(
            "categories",
            "category.decision.questions",
            "no selected category has typed-decision questions",
        ));
    }
    Ok(plans)
}

pub async fn run(cli: &Cli, args: &DecisionRunArgs) -> Result<DecisionRunCommandSummary, CliError> {
    let started = Instant::now();
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    let settings = settings(&loaded)?;
    let client: SystemOneClient = settings
        .build_client(|name| loaded.env.resolve_secret(name))
        .map_err(|error| config_error("app.toml", "decision", error.to_string()))?
        .ok_or_else(|| config_error("app.toml", "decision.enabled", "decision is disabled"))?;
    let execution_key = client.execution_key();
    let plans = plans(&loaded, &settings, &execution_key)?;
    let max_requests = args.max_requests.unwrap_or(settings.max_requests_per_run);
    let needed = settings
        .concurrency
        .saturating_mul(u32::try_from(plans.len()).unwrap_or(u32::MAX));
    if cli.category.is_none() && max_requests < needed {
        return Err(config_error(
            "app.toml",
            "decision.max_requests_per_run",
            format!(
                "{max_requests} requests cannot give each of the {} categories a first window of \
                 {}; raise it to at least {needed}, or run one category at a time with --category",
                plans.len(),
                settings.concurrency
            ),
        ));
    }
    let execution = DecisionExecution {
        provider: settings.provider.as_str().to_string(),
        requested_model: client.settings().model.clone(),
        laya_max_len: (settings.provider == Provider::Laya).then_some(settings.laya_max_len),
    };
    let pool = open_write_storage(&loaded).await?;
    let deps = build_decision_run_deps(&pool, Arc::new(client), &settings, execution);
    let summary = DecisionRunFlow::new(deps)
        .run(&DecisionRunOptions {
            categories: plans,
            max_requests,
            window_hours: args.window_hours.unwrap_or(settings.window_hours),
        })
        .await;
    Ok(DecisionRunCommandSummary::new(
        &summary,
        execution_key,
        max_requests,
        started,
    ))
}
