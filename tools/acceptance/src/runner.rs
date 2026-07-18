use std::{collections::HashSet, time::Instant};

use crate::{Lane, LaneReport, MatrixReport, Profile, RunOptions, Status};

mod checks;
mod executor;
mod lanes;
mod redact;
mod resources;
mod util;

use checks::{validate_version, workspace_version};
use executor::LaneExecutor;
use util::resolve_target_dir;

const SCHEMA_VERSION: u32 = 1;

pub fn run_matrix(options: RunOptions) -> Result<MatrixReport, String> {
    let started = Instant::now();
    let repo_root = options
        .repo_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve repo root {:?}: {error}", options.repo_root))?;
    if !repo_root.join("Cargo.toml").is_file() {
        return Err(format!(
            "repo root does not contain Cargo.toml: {}",
            repo_root.display()
        ));
    }

    let target_dir = resolve_target_dir(&repo_root, options.target_dir.as_deref());
    let expected_version = match options.expected_version.as_deref() {
        Some(version) => validate_version(version)?,
        None => workspace_version(&repo_root)?,
    };
    let (profile, lanes) = selected_lanes(options.profile, &options.lanes);
    let mut reports = Vec::with_capacity(lanes.len());
    let mut failed = false;

    for lane in lanes {
        if failed && options.fail_fast {
            reports.push(LaneReport {
                lane,
                status: Status::Skipped,
                duration_ms: 0,
                steps: Vec::new(),
            });
            continue;
        }
        let report = run_lane(
            lane,
            &repo_root,
            &target_dir,
            &expected_version,
            options.dry_run,
            options.fail_fast,
        );
        failed |= report.status == Status::Failed;
        reports.push(report);
    }

    let status = if options.dry_run {
        Status::Planned
    } else if failed {
        Status::Failed
    } else {
        Status::Passed
    };

    Ok(MatrixReport {
        schema_version: SCHEMA_VERSION,
        profile,
        dry_run: options.dry_run,
        status,
        repo_root: repo_root.display().to_string(),
        target_dir: target_dir.display().to_string(),
        expected_version,
        duration_ms: started.elapsed().as_millis(),
        lanes: reports,
    })
}

fn selected_lanes(profile: Option<Profile>, explicit: &[Lane]) -> (Option<Profile>, Vec<Lane>) {
    if explicit.is_empty() {
        let profile = profile.unwrap_or(Profile::Local);
        return (Some(profile), profile.lanes());
    }

    let mut seen = HashSet::new();
    let lanes = explicit
        .iter()
        .copied()
        .filter(|lane| seen.insert(*lane))
        .collect();
    (None, lanes)
}

fn run_lane(
    lane: Lane,
    repo_root: &std::path::Path,
    target_dir: &std::path::Path,
    expected_version: &str,
    dry_run: bool,
    fail_fast: bool,
) -> LaneReport {
    let started = Instant::now();
    let mut executor =
        LaneExecutor::new(repo_root, target_dir, expected_version, dry_run, fail_fast);
    lanes::run(lane, &mut executor);
    let status = executor.status();
    LaneReport {
        lane,
        status,
        duration_ms: started.elapsed().as_millis(),
        steps: executor.into_steps(),
    }
}

#[cfg(test)]
mod tests {
    use crate::{Lane, Profile};

    use super::selected_lanes;

    #[test]
    fn explicit_lanes_are_deduplicated_without_reordering() {
        let (profile, lanes) = selected_lanes(
            Some(Profile::Full),
            &[Lane::Sqlite, Lane::Static, Lane::Sqlite],
        );
        assert_eq!(profile, None);
        assert_eq!(lanes, vec![Lane::Sqlite, Lane::Static]);
    }
}
