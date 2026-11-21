use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

use super::super::{
    executor::LaneExecutor,
    process::run_bounded,
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
        wait_for_scheduler_job(&container, executor.step_timeout());
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

/// Log lines read per poll; the smoke job's error is among the first lines.
const SCHEDULER_SMOKE_LOG_TAIL: &str = "200";

/// Polls the container's logs until the smoke job has run or the timeout
/// passes; the logs step and its contract check report the outcome.
fn wait_for_scheduler_job(container: &str, step_timeout: Duration) {
    wait_for_marker(
        || {
            let mut command = Command::new("docker");
            command.args(["logs", "--tail", SCHEDULER_SMOKE_LOG_TAIL, container]);
            command
        },
        SCHEDULER_SMOKE_MARKER,
        SCHEDULER_SMOKE_TIMEOUT,
        step_timeout,
    );
}

/// Runs `poll` until its output contains `marker` or `total` has passed.
/// Every poll goes through [`run_bounded`] (capped output, killed process
/// tree) with the smaller of the remaining time and `step_timeout`, so one
/// hanging poll cannot outlive the deadline (R3-15). Returns whether the
/// marker was seen.
fn wait_for_marker(
    poll: impl Fn() -> Command,
    marker: &str,
    total: Duration,
    step_timeout: Duration,
) -> bool {
    let deadline = Instant::now() + total;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let seen = run_bounded(poll(), remaining.min(step_timeout)).is_ok_and(|finished| {
            finished.stdout.text.contains(marker) || finished.stderr.text.contains(marker)
        });
        if seen {
            return true;
        }
        thread::sleep(
            Duration::from_secs(1).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        time::{Duration, Instant},
    };

    use super::wait_for_marker;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn marker_in_either_stream_ends_the_wait() {
        assert!(wait_for_marker(
            || sh("echo ran: marker >&2"),
            "marker",
            Duration::from_secs(10),
            Duration::from_secs(10),
        ));
    }

    /// A poll that never returns is killed at the overall deadline.
    #[test]
    fn a_hanging_poll_does_not_outlive_the_deadline() {
        let started = Instant::now();
        assert!(!wait_for_marker(
            || sh("sleep 30"),
            "marker",
            Duration::from_secs(1),
            Duration::from_secs(60),
        ));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }
}
