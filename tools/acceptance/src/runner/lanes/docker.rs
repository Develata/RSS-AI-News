use std::{thread, time::Duration};

use super::super::{
    executor::LaneExecutor,
    resources::{DockerCleanup, prepare_smoke_workspace},
    util::{program_exists, strings},
};

pub(super) fn run(executor: &mut LaneExecutor<'_>) {
    let docker_present = program_exists("docker");
    executor.prerequisite("docker-cli", "docker CLI in PATH", docker_present);
    if !executor.dry_run() && !docker_present {
        return;
    }
    executor.command("docker-info", "docker", &strings(["info"]), &[], 0);

    let suffix = std::process::id();
    let runtime_tag = format!("rss-ai-news-acceptance:{suffix}-runtime");
    let debug_tag = format!("rss-ai-news-acceptance:{suffix}-debug");
    let scheduler_tag = format!("rss-ai-news-acceptance:{suffix}-scheduler");
    let container = format!("rss-ai-news-acceptance-{suffix}");
    let cleanup = DockerCleanup::new(
        executor.repo_root(),
        container.clone(),
        vec![
            runtime_tag.clone(),
            debug_tag.clone(),
            scheduler_tag.clone(),
        ],
        executor.dry_run(),
    );

    for (target, tag) in [
        ("runtime", runtime_tag.as_str()),
        ("debug", debug_tag.as_str()),
        ("scheduler", scheduler_tag.as_str()),
    ] {
        executor.command(
            &format!("docker-build-{target}"),
            "docker",
            &strings([
                "build",
                "--target",
                target,
                "-t",
                tag,
                "-f",
                "docker/Dockerfile",
                ".",
            ]),
            &[],
            0,
        );
    }
    executor.command(
        "docker-runtime-help",
        "docker",
        &strings(["run", "--rm", &runtime_tag, "--help"]),
        &[],
        0,
    );
    executor.command(
        "docker-debug-help",
        "docker",
        &strings(["run", "--rm", &debug_tag, "--help"]),
        &[],
        0,
    );
    executor.command(
        "docker-scheduler-start",
        "docker",
        &strings([
            "run",
            "-d",
            "--name",
            &container,
            "-e",
            "RSS_CRON_SCHEDULE=0 0 1 1 *",
            "-e",
            "RSS_CRON_COMMAND=--help",
            &scheduler_tag,
        ]),
        &[],
        0,
    );
    if !executor.dry_run() && executor.can_continue() {
        thread::sleep(Duration::from_secs(3));
    }
    let inspect = executor.command(
        "docker-scheduler-running",
        "docker",
        &strings(["inspect", "--format", "{{.State.Running}}", &container]),
        &[],
        0,
    );
    if let Some(output) = inspect {
        executor.check(
            "docker-scheduler-running-contract",
            "scheduler container remains running after boot",
            (output.stdout.trim() == "true")
                .then_some(())
                .ok_or_else(|| format!("docker inspect returned {:?}", output.stdout.trim())),
        );
    }
    executor.command(
        "docker-scheduler-logs",
        "docker",
        &strings(["logs", &container]),
        &[],
        0,
    );
    let Some(smoke) = prepare_smoke_workspace(
        executor,
        "docker",
        "docker-workspace",
        "create isolated Docker smoke config",
        false,
    ) else {
        cleanup.finish(executor);
        return;
    };
    let mount = format!("{}:/app/configs:ro", smoke.path().join("configs").display());
    executor.command(
        "docker-validate-config-exit",
        "docker",
        &strings([
            "run",
            "--rm",
            "-v",
            &mount,
            &runtime_tag,
            "--config-dir",
            "/app/configs",
            "validate-config",
        ]),
        &[],
        78,
    );

    cleanup.finish(executor);
}
