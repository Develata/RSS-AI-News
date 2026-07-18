use std::env;

use super::super::{
    executor::LaneExecutor,
    resources::prepare_smoke_workspace,
    util::{program_exists, release_binary, strings},
};

pub(super) fn run(executor: &mut LaneExecutor<'_>) {
    let docker_present = program_exists("docker");
    executor.prerequisite("postgres-docker-cli", "docker CLI in PATH", docker_present);
    let database_url = env::var("DATABASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    executor.prerequisite(
        "postgres-url",
        "non-empty DATABASE_URL pointing to a disposable PostgreSQL database",
        database_url.is_some(),
    );
    if !executor.dry_run() && (!docker_present || database_url.is_none()) {
        return;
    }

    executor.command("postgres-docker-info", "docker", &strings(["info"]), &[], 0);
    executor.command(
        "postgres-storage-tests",
        "cargo",
        &strings([
            "test",
            "-p",
            "rss-ai-news-storage",
            "--locked",
            "--",
            "--include-ignored",
            "--test-threads=1",
        ]),
        &[],
        0,
    );
    executor.command(
        "postgres-release-build",
        "cargo",
        &strings(["build", "--release", "--locked", "--bin", "rss-ai-news"]),
        &[],
        0,
    );
    let Some(smoke) = prepare_smoke_workspace(
        executor,
        "postgres",
        "postgres-workspace",
        "create isolated PostgreSQL smoke config",
        true,
    ) else {
        return;
    };

    let binary = release_binary(executor.target_dir());
    let mut args = vec![
        "--config-dir".to_string(),
        smoke.path().join("configs").display().to_string(),
        "migrate".to_string(),
        "run".to_string(),
    ];
    let url = database_url.as_deref().unwrap_or("$DATABASE_URL");
    executor.command(
        "postgres-migrate-run",
        &binary,
        &args,
        &[("DATABASE_URL", url)],
        0,
    );
    *args.last_mut().expect("migrate action") = "check".to_string();
    executor.command(
        "postgres-migrate-check",
        &binary,
        &args,
        &[("DATABASE_URL", url)],
        0,
    );
}
