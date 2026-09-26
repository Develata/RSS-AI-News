use std::path::Path;

use super::{
    super::{
        checks::{assert_published_after, assert_single_json_document},
        executor::LaneExecutor,
        resources::prepare_smoke_workspace,
        util::{release_binary, strings},
    },
    release::verify_binary_identity,
};

const EXPLICIT_CUTOFF: &str = "2000-01-01T00:00:00Z";

/// Network-free commands whose JSON output contract (docs/plan/11 §5) is
/// checked on the real release binary: `(step id, args, expected exit)`.
/// They cover a success summary, a summary-reported failure and an early
/// `Err`, each of which must print exactly one JSON document.
const JSON_CONTRACT_SMOKES: [(&str, &[&str], i32); 3] = [
    (
        "json-contract-publish-empty",
        &["--category", "ai", "publish", "--local-only"],
        0,
    ),
    (
        "json-contract-reindex-abort-missing",
        &["reindex", "--abort", "999999"],
        1,
    ),
    (
        "json-contract-rebuild-report-missing",
        &[
            "--category",
            "ai",
            "rebuild-report",
            "--publish-id",
            "999999",
        ],
        1,
    ),
];

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
    executor.product_command("sqlite-migrate-run", &binary, &args, smoke.path(), &[], 0);
    let mut args = base.clone();
    args.extend(strings(["migrate", "check"]));
    executor.product_command("sqlite-migrate-check", &binary, &args, smoke.path(), &[], 0);
    verify_binary_identity(executor, &binary);

    let args = recent_entries_args(&base, None);
    let default_output = executor.product_command(
        "recent-entries-default",
        &binary,
        &args,
        smoke.path(),
        &[],
        0,
    );
    if let Some(output) = default_output {
        executor.check(
            "recent-entries-default-contract",
            "omitted --published-after yields summary.published_after = null",
            assert_published_after(&output.stdout, None),
        );
    }

    let args = recent_entries_args(&base, Some(EXPLICIT_CUTOFF));
    let explicit_output = executor.product_command(
        "recent-entries-explicit-cutoff",
        &binary,
        &args,
        smoke.path(),
        &[],
        0,
    );
    if let Some(output) = explicit_output {
        executor.check(
            "recent-entries-explicit-contract",
            "explicit --published-after is reflected in the JSON contract",
            assert_published_after(&output.stdout, Some(EXPLICIT_CUTOFF)),
        );
    }

    for (id, command, expected_exit) in JSON_CONTRACT_SMOKES {
        let args = json_contract_args(&base, command);
        if let Some(output) =
            executor.product_command(id, &binary, &args, smoke.path(), &[], expected_exit)
        {
            executor.check(
                &format!("{id}-contract"),
                "stdout is exactly one JSON document whose status matches the exit code",
                assert_single_json_document(&output.stdout, output.stdout_truncated, expected_exit),
            );
        }
    }
}

fn plan_commands(executor: &mut LaneExecutor<'_>, smoke: &Path) {
    let binary = release_binary(executor.target_dir());
    let base = smoke_base(smoke);
    let mut args = base.clone();
    args.extend(strings(["migrate", "run"]));
    executor.product_command("sqlite-migrate-run", &binary, &args, smoke, &[], 0);
    let mut args = base.clone();
    args.extend(strings(["migrate", "check"]));
    executor.product_command("sqlite-migrate-check", &binary, &args, smoke, &[], 0);
    verify_binary_identity(executor, &binary);

    let args = recent_entries_args(&base, None);
    executor.product_command("recent-entries-default", &binary, &args, smoke, &[], 0);
    executor.check(
        "recent-entries-default-contract",
        "omitted --published-after yields summary.published_after = null",
        Ok(()),
    );
    let args = recent_entries_args(&base, Some(EXPLICIT_CUTOFF));
    executor.product_command(
        "recent-entries-explicit-cutoff",
        &binary,
        &args,
        smoke,
        &[],
        0,
    );
    executor.check(
        "recent-entries-explicit-contract",
        "explicit --published-after is reflected in the JSON contract",
        Ok(()),
    );
    for (id, command, expected_exit) in JSON_CONTRACT_SMOKES {
        executor.product_command(
            id,
            &binary,
            &json_contract_args(&base, command),
            smoke,
            &[],
            expected_exit,
        );
        executor.check(
            &format!("{id}-contract"),
            "stdout is exactly one JSON document whose status matches the exit code",
            Ok(()),
        );
    }
}

fn json_contract_args(base: &[String], command: &[&str]) -> Vec<String> {
    let mut args = base.to_vec();
    args.extend(strings(["--output-format", "json"]));
    args.extend(command.iter().map(|arg| arg.to_string()));
    args
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
