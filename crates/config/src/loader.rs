use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    AppConfig, CategoryConfig, CliOverrides, ConfigError, EnvConfig, compute_config_sha256, env,
    validate,
};

type CategoryTomlContents = Vec<(String, String)>;
type LoadedCategories = (Vec<CategoryConfig>, CategoryTomlContents);

#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub env: EnvConfig,
    pub app: AppConfig,
    pub categories: Vec<CategoryConfig>,
    pub config_sha256: String,
    pub cli_overrides: CliOverrides,
}

impl LoadedConfig {
    pub fn categories_filtered(&self) -> impl Iterator<Item = &CategoryConfig> {
        self.categories.iter().filter(|category| {
            self.cli_overrides
                .category_filter
                .as_deref()
                .is_none_or(|filter| category.category.key == filter)
        })
    }
}

pub fn load(
    config_dir: &Path,
    env_file: Option<&Path>,
    cli_overrides: CliOverrides,
) -> Result<LoadedConfig, ConfigError> {
    let env = env::load(env_file)?;

    let app_path = config_dir.join("app.toml");
    let app_content = read_required_file(&app_path)?;
    let mut app: AppConfig =
        toml::from_str(&app_content).map_err(|err| ConfigError::ParseFailed {
            path: app_path.display().to_string(),
            reason: err.to_string(),
        })?;

    let (categories, category_contents) = load_categories(&config_dir.join("categories"))?;

    cli_overrides.apply_to_app(&mut app);
    validate::run_general_checks(&app, &categories, &env)?;

    let config_sha256 = compute_config_sha256(&app_content, &category_contents);

    Ok(LoadedConfig {
        env,
        app,
        categories,
        config_sha256,
        cli_overrides,
    })
}

fn load_categories(categories_dir: &Path) -> Result<LoadedCategories, ConfigError> {
    if !categories_dir.exists() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(categories_dir).map_err(|err| ConfigError::ParseFailed {
        path: categories_dir.display().to_string(),
        reason: err.to_string(),
    })? {
        let path = entry
            .map_err(|err| ConfigError::ParseFailed {
                path: categories_dir.display().to_string(),
                reason: err.to_string(),
            })?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            paths.push(path);
        }
    }
    paths.sort();

    let mut categories = Vec::with_capacity(paths.len());
    let mut contents = Vec::with_capacity(paths.len());
    for path in paths {
        let content = read_required_file(&path)?;
        let category: CategoryConfig =
            toml::from_str(&content).map_err(|err| ConfigError::ParseFailed {
                path: path.display().to_string(),
                reason: err.to_string(),
            })?;
        let filename = filename(&path)?;
        contents.push((filename, content));
        categories.push(category);
    }

    Ok((categories, contents))
}

fn read_required_file(path: &Path) -> Result<String, ConfigError> {
    fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ConfigError::FileNotFound {
                path: path.display().to_string(),
            }
        } else {
            ConfigError::ParseFailed {
                path: path.display().to_string(),
                reason: err.to_string(),
            }
        }
    })
}

fn filename(path: &Path) -> Result<String, ConfigError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(String::from)
        .ok_or_else(|| ConfigError::ParseFailed {
            path: path.display().to_string(),
            reason: "category filename is not valid UTF-8".to_string(),
        })
}

#[allow(dead_code)]
fn _assert_pathbuf_send_sync(_: PathBuf) {}
