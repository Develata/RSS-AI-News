use super::super::{
    checks::{check_acceptance_documents, check_tooling_dependency_boundary},
    executor::LaneExecutor,
    util::strings,
};

pub(super) fn run(executor: &mut LaneExecutor<'_>) {
    executor.command(
        "fmt",
        "cargo",
        &strings(["fmt", "--all", "--", "--check"]),
        &[],
        0,
    );
    executor.command(
        "clippy",
        "cargo",
        &strings([
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ]),
        &[],
        0,
    );
    executor.command(
        "swallowed-errors",
        "bash",
        &strings([".ci/check_swallowed_errors.sh"]),
        &[],
        0,
    );
    executor.command(
        "dependency-policy",
        "bash",
        &strings([".ci/check_dependency_security_policy.sh"]),
        &[],
        0,
    );
    executor.check(
        "tooling-boundary",
        "acceptance tooling has no product-crate dependency",
        check_tooling_dependency_boundary(executor.repo_root()),
    );
    executor.check(
        "acceptance-docs",
        "all acceptance case documents declare a valid current status",
        check_acceptance_documents(executor.repo_root()),
    );
}
