use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use super::{executor::LaneExecutor, util::strings};

pub(crate) fn prepare_smoke_config(
    repo_root: &Path,
    smoke_root: &Path,
    postgres: bool,
) -> Result<(), String> {
    let config_dir = smoke_root.join("configs");
    let category_dir = config_dir.join("categories");
    fs::create_dir_all(&category_dir)
        .map_err(|error| format!("cannot create smoke config dir: {error}"))?;
    fs::create_dir_all(smoke_root.join("data"))
        .map_err(|error| format!("cannot create smoke data dir: {error}"))?;

    let app_source = repo_root.join("configs/app.toml.example");
    let mut app = fs::read_to_string(&app_source)
        .map_err(|error| format!("cannot read {}: {error}", app_source.display()))?;
    if postgres {
        app = app.replacen("driver = \"sqlite\"", "driver = \"postgres\"", 1);
        if !app.contains("driver = \"postgres\"") {
            return Err("could not set database.driver=postgres in smoke config".to_string());
        }
    }
    fs::write(config_dir.join("app.toml"), app)
        .map_err(|error| format!("cannot write smoke app.toml: {error}"))?;
    fs::copy(
        repo_root.join("configs/categories/ai.toml.example"),
        category_dir.join("ai.toml"),
    )
    .map_err(|error| format!("cannot copy smoke category config: {error}"))?;
    Ok(())
}

pub(crate) fn prepare_smoke_workspace(
    executor: &mut LaneExecutor<'_>,
    label: &str,
    step_id: &str,
    description: &str,
    postgres: bool,
) -> Option<SmokeWorkspace> {
    if !executor.can_continue() {
        return None;
    }
    let smoke = SmokeWorkspace::new(label, executor.dry_run());
    let prepare = if executor.dry_run() {
        Ok(())
    } else {
        prepare_smoke_config(executor.repo_root(), smoke.path(), postgres)
    };
    executor.check(step_id, description, prepare);
    executor.can_continue().then_some(smoke)
}

pub(crate) struct SmokeWorkspace {
    path: PathBuf,
    dry_run: bool,
}

impl SmokeWorkspace {
    pub(crate) fn new(label: &str, dry_run: bool) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        Self {
            path: env::temp_dir().join(format!(
                "rss-ai-news-acceptance-{label}-{}-{nonce}",
                std::process::id()
            )),
            dry_run,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SmokeWorkspace {
    fn drop(&mut self) {
        if !self.dry_run {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}

pub(crate) struct DockerCleanup {
    repo_root: PathBuf,
    container: String,
    images: Vec<String>,
    dry_run: bool,
    finished: bool,
}

impl DockerCleanup {
    pub(crate) fn new(
        repo_root: &Path,
        container: String,
        images: Vec<String>,
        dry_run: bool,
    ) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            container,
            images,
            dry_run,
            finished: false,
        }
    }

    pub(crate) fn finish(mut self, executor: &mut LaneExecutor<'_>) {
        executor.cleanup_command(
            "docker-clean-container",
            "docker",
            &strings(["rm", "-f", &self.container]),
            0,
        );
        let mut args = strings(["image", "rm", "-f"]);
        args.extend(self.images.clone());
        executor.cleanup_command("docker-clean-images", "docker", &args, 0);
        self.finished = true;
    }

    fn best_effort(&self) {
        if self.dry_run {
            return;
        }
        drop(
            Command::new("docker")
                .args(["rm", "-f", &self.container])
                .current_dir(&self.repo_root)
                .output(),
        );
        let mut command = Command::new("docker");
        command.args(["image", "rm", "-f"]);
        command.args(&self.images).current_dir(&self.repo_root);
        drop(command.output());
    }
}

impl Drop for DockerCleanup {
    fn drop(&mut self) {
        if !self.finished {
            self.best_effort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::prepare_smoke_workspace;
    use crate::runner::executor::LaneExecutor;

    #[test]
    fn fail_fast_failure_prevents_smoke_workspace_creation() {
        let mut executor =
            LaneExecutor::new(Path::new("."), Path::new("target"), "0.7.1", false, true);
        executor.check("forced-failure", "forced failure", Err("boom".to_string()));
        let workspace = prepare_smoke_workspace(
            &mut executor,
            "must-not-exist",
            "workspace",
            "workspace",
            false,
        );
        assert!(workspace.is_none());
    }
}
