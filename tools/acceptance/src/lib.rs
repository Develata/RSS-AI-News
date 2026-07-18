use std::{fmt, path::PathBuf};

use clap::ValueEnum;
use serde::Serialize;

pub mod runner;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Lane {
    Static,
    Workspace,
    Sqlite,
    Postgres,
    Docker,
    Release,
}

impl Lane {
    pub const ALL: [Self; 6] = [
        Self::Static,
        Self::Workspace,
        Self::Sqlite,
        Self::Postgres,
        Self::Docker,
        Self::Release,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Workspace => "workspace",
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
            Self::Docker => "docker",
            Self::Release => "release",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Static => "rustfmt, Clippy, swallowed-error and dependency policy gates",
            Self::Workspace => "locked workspace build and tests",
            Self::Sqlite => "release binary, SQLite migrations and CLI contract smokes",
            Self::Postgres => "PostgreSQL ignored tests and CLI migration parity",
            Self::Docker => "runtime/debug/scheduler image builds and container smokes",
            Self::Release => "workspace, lockfile, README and binary version identity",
        }
    }
}

impl fmt::Display for Lane {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    Local,
    Full,
}

impl Profile {
    pub const fn id(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Full => "full",
        }
    }

    pub fn lanes(self) -> Vec<Lane> {
        match self {
            Self::Local => vec![Lane::Static, Lane::Workspace, Lane::Sqlite, Lane::Release],
            Self::Full => Lane::ALL.to_vec(),
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub repo_root: PathBuf,
    pub target_dir: Option<PathBuf>,
    pub lanes: Vec<Lane>,
    pub profile: Option<Profile>,
    pub expected_version: Option<String>,
    pub dry_run: bool,
    pub fail_fast: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Planned,
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub id: String,
    pub command: String,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaneReport {
    pub lane: Lane,
    pub status: Status,
    pub duration_ms: u128,
    pub steps: Vec<StepReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatrixReport {
    pub schema_version: u32,
    pub profile: Option<Profile>,
    pub dry_run: bool,
    pub status: Status,
    pub repo_root: String,
    pub target_dir: String,
    pub expected_version: String,
    pub duration_ms: u128,
    pub lanes: Vec<LaneReport>,
}

impl MatrixReport {
    pub fn succeeded(&self) -> bool {
        matches!(self.status, Status::Passed | Status::Planned)
    }
}

#[cfg(test)]
mod tests {
    use super::{Lane, Profile};

    #[test]
    fn local_profile_is_reproducible_without_external_services() {
        assert_eq!(
            Profile::Local.lanes(),
            vec![Lane::Static, Lane::Workspace, Lane::Sqlite, Lane::Release]
        );
    }

    #[test]
    fn full_profile_covers_every_registered_lane_once() {
        let lanes = Profile::Full.lanes();
        assert_eq!(lanes, Lane::ALL);
        let mut ids = lanes.iter().map(|lane| lane.id()).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Lane::ALL.len());
    }
}
