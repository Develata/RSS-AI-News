use rss_ai_news_domain::SecretString;
use rss_ai_news_observability::health::{
    CheckReport, HealthCheck, backlog_check::FailedBacklogCheck, config_check::ConfigCheck,
    db_check::DatabaseConnectivityCheck, disk_check::DiskSpaceCheck, github_check::GitHubPingCheck,
    lease_check::ExpiredLeaseCheck, migration_check::MigrationVersionCheck,
    openai_check::OpenAiPingCheck, rsshub_check::RsshubPingCheck, timezone_check::TimezoneCheck,
};
use rss_ai_news_runtime::doctor::deep_scan;

use crate::{
    args::{Cli, DoctorArgs},
    context_factory::build_doctor_deps,
    error::CliError,
    output::{DoctorCommandSummary, OutputWriter},
};

pub async fn run(cli: &Cli, args: &DoctorArgs, writer: &mut OutputWriter) -> Result<(), CliError> {
    let deps = build_doctor_deps(cli).await?;
    let app = &deps.loaded.app;
    let env = &deps.loaded.env;
    let min_free_bytes = 100 * 1024 * 1024;
    let github_token: Option<String> = if app.publish.github_owner.trim().is_empty()
        || app.publish.github_repo.trim().is_empty()
    {
        None
    } else {
        env.github_token
            .as_ref()
            .map(|secret| secret.expose_secret().to_owned())
    };
    let openai_api_key: Option<String> = env
        .openai_api_key
        .as_ref()
        .map(SecretString::expose_secret)
        .map(str::to_owned);
    let checks: Vec<Box<dyn HealthCheck>> = vec![
        Box::new(ConfigCheck::new(deps.loaded.clone())),
        Box::new(DatabaseConnectivityCheck::new(deps.pool.clone())),
        Box::new(MigrationVersionCheck::new(deps.pool.clone())),
        Box::new(OpenAiPingCheck::new(
            deps.http_client.clone(),
            env.openai_base_url.clone(),
            openai_api_key,
            app.ai.model.clone(),
            app.ai.enabled,
        )),
        Box::new(GitHubPingCheck::new(deps.http_client.clone(), github_token)),
        Box::new(RsshubPingCheck::new(
            deps.http_client.clone(),
            env.rsshub_base_url.clone(),
        )),
        Box::new(TimezoneCheck::new(app.publish.target_timezone.clone())),
        Box::new(DiskSpaceCheck::new(
            app.database.sqlite_path.clone(),
            min_free_bytes,
        )),
        Box::new(ExpiredLeaseCheck::new(deps.pool.clone())),
        Box::new(FailedBacklogCheck::new(deps.pool.clone())),
    ];

    let mut report = CheckReport::default();
    for check in checks {
        let name = check.name().to_string();
        let outcome = check.run().await;
        report.items.push((name, outcome));
    }

    let deep = if args.deep {
        Some(deep_scan::run(&deps.pool).await?)
    } else {
        None
    };
    let summary = DoctorCommandSummary::new(report, deep);
    let has_fail = summary.has_fail();
    writer
        .emit_success("doctor", &summary)
        .map_err(CliError::Io)?;
    if has_fail {
        Err(CliError::DoctorFailed)
    } else {
        Ok(())
    }
}
