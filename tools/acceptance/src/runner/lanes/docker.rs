use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

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
            SCHEDULER_SMOKE_SCHEDULE,
            "-e",
            SCHEDULER_SMOKE_COMMAND,
            &scheduler_tag,
        ]),
        &[],
        0,
    );
    if !executor.dry_run() && executor.can_continue() {
        wait_for_scheduler_job(&container);
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
    // supercronic logs to stderr; fold it into stdout for the contract check.
    let logs = executor.command(
        "docker-scheduler-logs",
        "sh",
        &strings(["-c", &format!("docker logs {container} 2>&1")]),
        &[],
        0,
    );
    if let Some(output) = logs {
        executor.check(
            "docker-scheduler-job-contract",
            "scheduler ran one job with its spaced argument intact",
            output
                .stdout
                .contains(SCHEDULER_SMOKE_MARKER)
                .then_some(())
                .ok_or_else(|| format!("scheduler logs lack {SCHEDULER_SMOKE_MARKER:?}")),
        );
    }
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

/// Seven fields (seconds first): fire every 2 s, so one job runs promptly.
const SCHEDULER_SMOKE_SCHEDULE: &str = "RSS_CRON_SCHEDULE=*/2 * * * * * *";
/// Side-effect free (clap rejects the extra positional before any config or
/// database access); the space proves the generated crontab line keeps a
/// quoted argument as one argv element.
const SCHEDULER_SMOKE_COMMAND: &str = "RSS_CRON_COMMAND=validate-config 'spaced arg'";
const SCHEDULER_SMOKE_MARKER: &str = "unexpected argument 'spaced arg'";
const SCHEDULER_SMOKE_TIMEOUT: Duration = Duration::from_secs(75);

/// Polls the container's logs until the smoke job has run or the timeout
/// passes; the logs step and its contract check report the outcome.
fn wait_for_scheduler_job(container: &str) {
    let deadline = Instant::now() + SCHEDULER_SMOKE_TIMEOUT;
    while Instant::now() < deadline {
        let ran = Command::new("docker")
            .args(["logs", container])
            .output()
            .is_ok_and(|output| {
                String::from_utf8_lossy(&output.stdout).contains(SCHEDULER_SMOKE_MARKER)
                    || String::from_utf8_lossy(&output.stderr).contains(SCHEDULER_SMOKE_MARKER)
            });
        if ran {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}
