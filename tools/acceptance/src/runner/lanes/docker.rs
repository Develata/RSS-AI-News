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
    // `None` in a dry run or after an earlier failure: the job-contract
    // check below is then recorded as planned or skipped by the executor.
    let job_seen = (!executor.dry_run() && executor.can_continue())
        .then(|| wait_for_scheduler_job(&container, executor.step_timeout()));
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
    // The verdict is whether the marker was seen before the 75 s deadline
    // (R3G-S15); a later log read only adds bounded diagnostics to a failure.
    let step_timeout = executor.step_timeout();
    executor.check(
        "docker-scheduler-job-contract",
        "scheduler ran one job with its spaced argument intact",
        scheduler_job_verdict(job_seen.unwrap_or(true), || {
            scheduler_log_tail(&container, step_timeout)
        }),
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
/// Log lines and time spent on the diagnostic read after a failed wait.
const SCHEDULER_DIAGNOSTIC_LOG_TAIL: &str = "50";
const SCHEDULER_DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(10);

/// The job contract from the wait's result alone: `seen` is whether the
/// marker appeared before the deadline. `diagnostic_logs` is read only on
/// failure and only for the error text, so logs read after the deadline can
/// never turn a missed deadline into a pass (R3G-S15).
fn scheduler_job_verdict(
    seen: bool,
    diagnostic_logs: impl FnOnce() -> String,
) -> Result<(), String> {
    if seen {
        return Ok(());
    }
    Err(format!(
        "scheduler logs lack {SCHEDULER_SMOKE_MARKER:?} within {}s; last log lines: {}",
        SCHEDULER_SMOKE_TIMEOUT.as_secs(),
        diagnostic_logs()
    ))
}

/// The container's last log lines for a failure message, bounded in lines
/// and time; a failed read is described, not propagated.
fn scheduler_log_tail(container: &str, step_timeout: Duration) -> String {
    let mut command = Command::new("docker");
    command.args(["logs", "--tail", SCHEDULER_DIAGNOSTIC_LOG_TAIL, container]);
    match run_bounded(command, SCHEDULER_DIAGNOSTIC_TIMEOUT.min(step_timeout)) {
        // supercronic logs to stderr.
        Ok(finished) => format!("{}{}", finished.stdout.text, finished.stderr.text),
        Err(error) => format!("(docker logs failed: {error})"),
    }
}

/// Polls the container's logs until the smoke job has run or the timeout
/// passes; returns whether the marker was seen in time.
fn wait_for_scheduler_job(container: &str, step_timeout: Duration) -> bool {
    wait_for_marker(
        || {
            let mut command = Command::new("docker");
            command.args(["logs", "--tail", SCHEDULER_SMOKE_LOG_TAIL, container]);
            command
        },
        SCHEDULER_SMOKE_MARKER,
        SCHEDULER_SMOKE_TIMEOUT,
        step_timeout,
    )
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

    use super::{SCHEDULER_SMOKE_MARKER, scheduler_job_verdict, wait_for_marker};

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

    /// A marker that only shows up after the deadline is a missed deadline.
    #[test]
    fn a_marker_after_the_deadline_is_not_seen() {
        let dir =
            std::env::temp_dir().join(format!("acceptance-late-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let log = dir.join("log");
        let writer = {
            let log = log.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                std::fs::write(&log, "late marker\n").expect("write late marker");
            })
        };
        let script = format!("cat {} 2>/dev/null || true", log.display());
        assert!(!wait_for_marker(
            || sh(&script),
            "marker",
            Duration::from_secs(1),
            Duration::from_secs(10),
        ));
        writer.join().expect("writer thread");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The verdict comes from the wait alone: logs that contain the marker
    /// after the deadline do not pass, and a pass reads no logs.
    #[test]
    fn job_verdict_ignores_logs_read_after_the_deadline() {
        let err = scheduler_job_verdict(false, || format!("ran: {SCHEDULER_SMOKE_MARKER}"))
            .expect_err("a missed deadline fails");
        assert!(err.contains("within 75s"), "{err}");
        scheduler_job_verdict(true, || panic!("logs are not read on success"))
            .expect("the marker was seen in time");
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
