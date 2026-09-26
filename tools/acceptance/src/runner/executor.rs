use std::{
    env,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use crate::{Status, StepReport};

use super::{
    process::run_bounded,
    redact::{is_sensitive_env_key, redact_output},
    util::{display_command, tail},
};

/// Run-wide execution policy shared by every lane.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecOptions {
    pub(crate) dry_run: bool,
    pub(crate) fail_fast: bool,
    /// Apply small-volume Cargo settings (serial jobs, no incremental, no dev
    /// debuginfo). Off by default: they change Cargo's fingerprint, forcing a
    /// full rebuild of the shared target dir for acceptance and again for the
    /// developer's next ordinary build.
    pub(crate) low_resource: bool,
    /// Wall-clock limit per step; the step's process tree is killed on expiry.
    pub(crate) step_timeout: Duration,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            fail_fast: false,
            low_resource: false,
            step_timeout: Duration::from_secs(60 * 60),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProcessOutput {
    pub(crate) stdout: String,
    /// Earlier stdout bytes were dropped by the capture cap; contract checks
    /// must not trust a truncated document.
    pub(crate) stdout_truncated: bool,
}

pub(crate) struct LaneExecutor<'a> {
    repo_root: &'a Path,
    target_dir: &'a Path,
    expected_version: &'a str,
    options: ExecOptions,
    failed: bool,
    inherited_sensitive_env: Vec<(String, String)>,
    steps: Vec<StepReport>,
}

impl<'a> LaneExecutor<'a> {
    pub(crate) fn new(
        repo_root: &'a Path,
        target_dir: &'a Path,
        expected_version: &'a str,
        options: ExecOptions,
    ) -> Self {
        Self {
            repo_root,
            target_dir,
            expected_version,
            options,
            failed: false,
            inherited_sensitive_env: env::vars_os()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .filter(|(key, _)| is_sensitive_env_key(key))
                .collect(),
            steps: Vec::new(),
        }
    }

    pub(crate) fn repo_root(&self) -> &Path {
        self.repo_root
    }

    pub(crate) fn target_dir(&self) -> &Path {
        self.target_dir
    }

    pub(crate) fn expected_version(&self) -> &str {
        self.expected_version
    }

    pub(crate) fn dry_run(&self) -> bool {
        self.options.dry_run
    }

    pub(crate) fn status(&self) -> Status {
        if self.options.dry_run {
            Status::Planned
        } else if self.failed {
            Status::Failed
        } else {
            Status::Passed
        }
    }

    pub(crate) fn into_steps(self) -> Vec<StepReport> {
        self.steps
    }

    pub(crate) fn can_continue(&self) -> bool {
        !self.failed || !self.options.fail_fast
    }

    pub(crate) fn command(
        &mut self,
        id: &str,
        program: impl AsRef<Path>,
        args: &[String],
        envs: &[(&str, &str)],
        expected_exit: i32,
    ) -> Option<ProcessOutput> {
        self.command_inner(id, program.as_ref(), args, envs, expected_exit, false)
    }

    pub(crate) fn cleanup_command(
        &mut self,
        id: &str,
        program: impl AsRef<Path>,
        args: &[String],
        expected_exit: i32,
    ) -> Option<ProcessOutput> {
        self.command_inner(id, program.as_ref(), args, &[], expected_exit, true)
    }

    fn command_inner(
        &mut self,
        id: &str,
        program: &Path,
        args: &[String],
        envs: &[(&str, &str)],
        expected_exit: i32,
        always_run: bool,
    ) -> Option<ProcessOutput> {
        let display = display_command(program, args, envs);
        if self.options.dry_run {
            self.steps.push(StepReport {
                id: id.to_string(),
                command: display,
                status: Status::Planned,
                exit_code: None,
                duration_ms: 0,
                stdout_tail: None,
                stderr_tail: None,
                error: None,
            });
            return None;
        }
        if !always_run && !self.can_continue() {
            return None;
        }

        let started = Instant::now();
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(self.repo_root)
            .env("CARGO_TARGET_DIR", self.target_dir);
        if self.options.low_resource {
            apply_small_volume_cargo_defaults(program, &mut command);
        }
        for (key, value) in envs {
            command.env(key, value);
        }

        match run_bounded(command, self.options.step_timeout) {
            Ok(finished) => {
                let code = finished.exit_code;
                let passed = !finished.timed_out && code == Some(expected_exit);
                let safe_stdout = self.redact(&finished.stdout.text, envs);
                let safe_stderr = self.redact(&finished.stderr.text, envs);
                if !passed {
                    self.failed = true;
                }
                let error = if finished.timed_out {
                    Some(format!(
                        "timed out after {}s; process group killed",
                        self.options.step_timeout.as_secs()
                    ))
                } else {
                    (!passed).then(|| format!("expected exit {expected_exit}, observed {:?}", code))
                };
                self.steps.push(StepReport {
                    id: id.to_string(),
                    command: display,
                    status: if passed {
                        Status::Passed
                    } else {
                        Status::Failed
                    },
                    exit_code: code,
                    duration_ms: started.elapsed().as_millis(),
                    stdout_tail: (!passed).then(|| tail(&safe_stdout)),
                    stderr_tail: (!passed).then(|| tail(&safe_stderr)),
                    error,
                });
                passed.then_some(ProcessOutput {
                    stdout: finished.stdout.text,
                    stdout_truncated: finished.stdout.truncated,
                })
            }
            Err(error) => {
                self.failed = true;
                self.steps.push(StepReport {
                    id: id.to_string(),
                    command: display,
                    status: Status::Failed,
                    exit_code: None,
                    duration_ms: started.elapsed().as_millis(),
                    stdout_tail: None,
                    stderr_tail: None,
                    error: Some(format!("cannot spawn command: {error}")),
                });
                None
            }
        }
    }

    /// Redacts explicit child env secrets and sensitive inherited parent env
    /// values, plus the generic URL-userinfo / key=value / Bearer patterns.
    fn redact(&self, text: &str, envs: &[(&str, &str)]) -> String {
        let inherited = self
            .inherited_sensitive_env
            .iter()
            .filter(|(key, _)| !envs.iter().any(|(explicit, _)| *explicit == key))
            .map(|(_, value)| value.as_str());
        redact_output(text, envs.iter().map(|(_, value)| *value).chain(inherited))
    }

    pub(crate) fn check(&mut self, id: &str, description: &str, result: Result<(), String>) {
        if self.options.dry_run {
            self.steps.push(StepReport {
                id: id.to_string(),
                command: description.to_string(),
                status: Status::Planned,
                exit_code: None,
                duration_ms: 0,
                stdout_tail: None,
                stderr_tail: None,
                error: None,
            });
            return;
        }
        if !self.can_continue() {
            return;
        }

        let (status, error) = match result {
            Ok(()) => (Status::Passed, None),
            Err(error) => {
                self.failed = true;
                // Check errors may quote child stdout; redact like step output.
                (Status::Failed, Some(tail(&self.redact(&error, &[]))))
            }
        };
        self.steps.push(StepReport {
            id: id.to_string(),
            command: description.to_string(),
            status,
            exit_code: None,
            duration_ms: 0,
            stdout_tail: None,
            stderr_tail: None,
            error,
        });
    }

    pub(crate) fn prerequisite(&mut self, id: &str, description: &str, present: bool) -> bool {
        self.check(
            id,
            description,
            present
                .then_some(())
                .ok_or_else(|| format!("missing prerequisite: {description}")),
        );
        present
    }
}

fn apply_small_volume_cargo_defaults(program: &Path, command: &mut Command) {
    if program.file_name().and_then(|name| name.to_str()) != Some("cargo") {
        return;
    }
    for (key, default) in [
        ("CARGO_BUILD_JOBS", "1"),
        ("CARGO_INCREMENTAL", "0"),
        ("CARGO_PROFILE_DEV_DEBUG", "0"),
    ] {
        if env::var_os(key).is_none() {
            command.env(key, default);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use super::{ExecOptions, LaneExecutor};
    use crate::Status;

    #[cfg(unix)]
    #[test]
    fn failed_command_evidence_redacts_child_environment_secret() {
        let secret = "postgres://alice:hunter2@db.example.test/rss";
        let mut executor = LaneExecutor::new(
            Path::new("."),
            Path::new("target"),
            "0.7.1",
            ExecOptions::default(),
        );
        executor.command(
            "leak-attempt",
            "sh",
            &[
                "-c".to_string(),
                "printf '%s' \"$DATABASE_URL\" >&2; exit 7".to_string(),
            ],
            &[("DATABASE_URL", secret)],
            0,
        );
        let report = executor.into_steps().pop().expect("step report");
        let evidence = report.stderr_tail.expect("stderr evidence");
        assert!(!evidence.contains(secret));
        assert!(!evidence.contains("hunter2"));
        assert!(evidence.contains("[REDACTED]"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_command_evidence_redacts_inherited_environment_secret() {
        let secret = "inherited-aws-access-key-id";
        let mut executor = LaneExecutor::new(
            Path::new("."),
            Path::new("target"),
            "0.7.1",
            ExecOptions::default(),
        );
        executor.inherited_sensitive_env =
            vec![("AWS_ACCESS_KEY_ID".to_string(), secret.to_string())];
        executor.command(
            "inherited-leak-attempt",
            "sh",
            &[
                "-c".to_string(),
                format!("printf '%s' '{secret}' >&2; exit 7"),
            ],
            &[],
            0,
        );
        let report = executor.into_steps().pop().expect("step report");
        let evidence = report.stderr_tail.expect("stderr evidence");
        assert!(!evidence.contains(secret));
        assert!(evidence.contains("[REDACTED]"));
    }

    #[test]
    fn failed_check_error_is_redacted() {
        let secret = "inherited-aws-access-key-id";
        let mut executor = LaneExecutor::new(
            Path::new("."),
            Path::new("target"),
            "0.7.1",
            ExecOptions::default(),
        );
        executor.inherited_sensitive_env =
            vec![("AWS_ACCESS_KEY_ID".to_string(), secret.to_string())];
        executor.check("contract", "stdout contract", Err(format!("got {secret}")));
        let report = executor.into_steps().pop().expect("step report");
        let error = report.error.expect("check error");
        assert!(!error.contains(secret));
        assert!(error.contains("[REDACTED]"));
    }

    #[cfg(unix)]
    #[test]
    fn step_timeout_fails_the_step() {
        let mut executor = LaneExecutor::new(
            Path::new("."),
            Path::new("target"),
            "0.7.1",
            ExecOptions {
                step_timeout: Duration::from_millis(200),
                ..ExecOptions::default()
            },
        );
        let output = executor.command(
            "hangs",
            "sh",
            &["-c".to_string(), "sleep 30".to_string()],
            &[],
            0,
        );
        assert!(output.is_none());
        let report = executor.into_steps().pop().expect("step report");
        assert_eq!(report.status, Status::Failed);
        assert!(report.error.unwrap().contains("timed out"));
    }
}
