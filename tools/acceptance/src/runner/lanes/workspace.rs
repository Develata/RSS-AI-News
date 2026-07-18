use super::super::{executor::LaneExecutor, util::strings};

pub(super) fn run(executor: &mut LaneExecutor<'_>) {
    executor.command(
        "workspace-build",
        "cargo",
        &strings(["build", "--workspace", "--locked"]),
        &[],
        0,
    );
    executor.command(
        "workspace-test",
        "cargo",
        &strings(["test", "--workspace", "--locked"]),
        &[],
        0,
    );
}
