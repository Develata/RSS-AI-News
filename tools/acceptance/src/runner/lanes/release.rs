use std::path::Path;

use super::super::{
    checks::{
        check_deployment_image_versions, check_lockfile_versions, check_readme_release_identity,
        workspace_version,
    },
    executor::LaneExecutor,
    util::{release_binary, strings},
};

pub(super) fn run(executor: &mut LaneExecutor<'_>) {
    executor.command(
        "release-build",
        "cargo",
        &strings(["build", "--release", "--locked", "--bin", "rss-ai-news"]),
        &[],
        0,
    );
    let expected = executor.expected_version().to_string();
    executor.check(
        "workspace-version",
        "workspace.package.version matches --expected-version",
        workspace_version(executor.repo_root()).and_then(|actual| {
            (actual == expected)
                .then_some(())
                .ok_or_else(|| format!("workspace version {actual}, expected {expected}"))
        }),
    );
    executor.check(
        "lockfile-versions",
        "all rss-ai-news workspace packages in Cargo.lock match --expected-version",
        check_lockfile_versions(executor.repo_root(), &expected),
    );
    executor.check(
        "readme-version",
        "README declares the candidate version and acceptance command",
        check_readme_release_identity(executor.repo_root(), &expected),
    );
    executor.check(
        "deployment-versions",
        "compose default image version and deployment docs name the candidate version",
        check_deployment_image_versions(executor.repo_root(), &expected),
    );
    // The candidate's release report carries the GitHub Release notes: the
    // same extraction the release workflow's github-release job runs, so a
    // tag without them fails verify-release before any image is pushed.
    executor.command(
        "release-notes",
        "sh",
        &[
            ".github/scripts/release-github.sh".to_string(),
            "notes".to_string(),
            expected.clone(),
            format!("docs/reports/releases/v{expected}.md"),
        ],
        &[],
        0,
    );
    executor.command(
        "git-diff-check",
        "git",
        &strings(["diff", "--check"]),
        &[],
        0,
    );
    let binary = release_binary(executor.target_dir());
    verify_binary_identity(executor, &binary);
}

pub(super) fn verify_binary_identity(executor: &mut LaneExecutor<'_>, binary: &Path) {
    let version = executor.command("binary-version", binary, &strings(["--version"]), &[], 0);
    if let Some(output) = version {
        let expected = format!("rss-ai-news {}", executor.expected_version());
        executor.check(
            "binary-version-contract",
            "release binary --version matches candidate version",
            (output.stdout.trim() == expected)
                .then_some(())
                .ok_or_else(|| {
                    format!(
                        "binary reported {:?}, expected {:?}",
                        output.stdout.trim(),
                        expected
                    )
                }),
        );
    }
    executor.command("binary-help", binary, &strings(["--help"]), &[], 0);
}
