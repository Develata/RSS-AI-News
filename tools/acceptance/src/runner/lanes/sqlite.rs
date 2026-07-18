use std::path::Path;

use super::{
    super::{
        checks::assert_published_after,
        executor::LaneExecutor,
        resources::prepare_smoke_workspace,
        util::{release_binary, strings},
    },
    release::verify_binary_identity,
};

const EXPLICIT_CUTOFF: &str = "2000-01-01T00:00:00Z";

pub(super) fn run(executor: &mut LaneExecutor<'_>) {
    executor.command(
        "release-build",
        "cargo",
        &strings(["build", "--release", "--locked", "--bin", "rss-ai-news"]),
        &[],
        0,
    );
    let Some(smoke) = prepare_smoke_workspace(
        executor,
        "sqlite",
        "sqlite-workspace",
        "create isolated SQLite smoke workspace",
        false,
    ) else {
        return;
    };
    if executor.dry_run() {
        plan_commands(executor, smoke.path());
        return;
    }
    if !executor.can_continue() {
        return;
    }

    let binary = release_binary(executor.target_dir());
    let base = smoke_base(smoke.path());
    let mut args = base.clone();
    args.extend(strings(["migrate", "run"]));
    executor.command("sqlite-migrate-run", &binary, &args, &[], 0);
    let mut args = base.clone();
    args.extend(strings(["migrate", "check"]));
    executor.command("sqlite-migrate-check", &binary, &args, &[], 0);
    verify_binary_identity(executor, &binary);

    let args = recent_entries_args(&base, None);
    let default_output = executor.command("recent-entries-default", &binary, &args, &[], 0);
    if let Some(output) = default_output {
        executor.check(
            "recent-entries-default-contract",
            "omitted --published-after yields summary.published_after = null",
            assert_published_after(&output.stdout, None),
        );
    }

    let args = recent_entries_args(&base, Some(EXPLICIT_CUTOFF));
    let explicit_output =
        executor.command("recent-entries-explicit-cutoff", &binary, &args, &[], 0);
    if let Some(output) = explicit_output {
        executor.check(
            "recent-entries-explicit-contract",
            "explicit --published-after is reflected in the JSON contract",
            assert_published_after(&output.stdout, Some(EXPLICIT_CUTOFF)),
        );
    }
}

fn plan_commands(executor: &mut LaneExecutor<'_>, smoke: &Path) {
    let binary = release_binary(executor.target_dir());
    let base = smoke_base(smoke);
    let mut args = base.clone();
    args.extend(strings(["migrate", "run"]));
    executor.command("sqlite-migrate-run", &binary, &args, &[], 0);
    let mut args = base.clone();
    args.extend(strings(["migrate", "check"]));
    executor.command("sqlite-migrate-check", &binary, &args, &[], 0);
    verify_binary_identity(executor, &binary);

    let args = recent_entries_args(&base, None);
    executor.command("recent-entries-default", &binary, &args, &[], 0);
    executor.check(
        "recent-entries-default-contract",
        "omitted --published-after yields summary.published_after = null",
        Ok(()),
    );
    let args = recent_entries_args(&base, Some(EXPLICIT_CUTOFF));
    executor.command("recent-entries-explicit-cutoff", &binary, &args, &[], 0);
    executor.check(
        "recent-entries-explicit-contract",
        "explicit --published-after is reflected in the JSON contract",
        Ok(()),
    );
}

fn smoke_base(smoke: &Path) -> Vec<String> {
    vec![
        "--config-dir".to_string(),
        smoke.join("configs").display().to_string(),
        "--db-path".to_string(),
        smoke.join("data/smoke.db").display().to_string(),
    ]
}

fn recent_entries_args(base: &[String], published_after: Option<&str>) -> Vec<String> {
    let mut args = base.to_vec();
    args.extend(strings([
        "--category",
        "ai",
        "--output-format",
        "json",
        "recent-entries",
        "--discovered-after",
        "1970-01-01T00:00:00Z",
    ]));
    if let Some(cutoff) = published_after {
        args.extend(strings(["--published-after", cutoff]));
    }
    args.extend(strings(["--limit", "5"]));
    args
}

#[cfg(test)]
mod tests {
    use super::{EXPLICIT_CUTOFF, recent_entries_args};

    #[test]
    fn publication_cutoff_is_absent_by_default_and_explicit_when_requested() {
        let default = recent_entries_args(&[], None);
        assert!(!default.iter().any(|arg| arg == "--published-after"));
        let explicit = recent_entries_args(&[], Some(EXPLICIT_CUTOFF));
        let index = explicit
            .iter()
            .position(|arg| arg == "--published-after")
            .unwrap();
        assert_eq!(explicit[index + 1], EXPLICIT_CUTOFF);
    }
}
