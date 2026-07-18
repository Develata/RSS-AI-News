mod docker;
mod postgres;
mod release;
mod sqlite;
mod static_checks;
mod workspace;

use crate::Lane;

use super::executor::LaneExecutor;

pub(crate) fn run(lane: Lane, executor: &mut LaneExecutor<'_>) {
    match lane {
        Lane::Static => static_checks::run(executor),
        Lane::Workspace => workspace::run(executor),
        Lane::Sqlite => sqlite::run(executor),
        Lane::Postgres => postgres::run(executor),
        Lane::Docker => docker::run(executor),
        Lane::Release => release::run(executor),
    }
}
