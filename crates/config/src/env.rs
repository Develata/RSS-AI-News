use std::{
    env,
    path::{Path, PathBuf},
};

use crate::ConfigError;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvConfig {
    pub openai_api_key: Option<String>,
    pub openai_base_url: Option<String>,
    pub github_token: Option<String>,
    pub rsshub_base_url: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub database_url: Option<String>,
}

pub fn load(env_file: Option<&Path>) -> Result<EnvConfig, ConfigError> {
    let file_values = load_file_values(env_file)?;

    Ok(EnvConfig {
        openai_api_key: value("OPENAI_API_KEY", &file_values),
        openai_base_url: value("OPENAI_BASE_URL", &file_values),
        github_token: value("GITHUB_TOKEN", &file_values),
        rsshub_base_url: value("RSSHUB_BASE_URL", &file_values),
        http_proxy: value("HTTP_PROXY", &file_values),
        https_proxy: value("HTTPS_PROXY", &file_values),
        database_url: value("DATABASE_URL", &file_values),
    })
}

fn load_file_values(env_file: Option<&Path>) -> Result<Vec<(String, String)>, ConfigError> {
    let default_path;
    let Some(path) = env_file else {
        default_path = PathBuf::from(".env");
        if !default_path.exists() {
            return Ok(Vec::new());
        }
        return load_existing_file(&default_path);
    };

    if !path.exists() {
        return Err(ConfigError::FileNotFound {
            path: path.display().to_string(),
        });
    }

    load_existing_file(path)
}

fn load_existing_file(path: &Path) -> Result<Vec<(String, String)>, ConfigError> {
    let mut values = Vec::new();
    for item in dotenvy::from_path_iter(path).map_err(|err| ConfigError::ParseFailed {
        path: path.display().to_string(),
        reason: err.to_string(),
    })? {
        let (key, value) = item.map_err(|err| ConfigError::ParseFailed {
            path: path.display().to_string(),
            reason: err.to_string(),
        })?;
        values.push((key, value));
    }
    Ok(values)
}

fn value(key: &str, file_values: &[(String, String)]) -> Option<String> {
    env::var(key)
        .ok()
        .or_else(|| {
            file_values
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        })
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::{fs, time::SystemTime};

    use super::*;

    #[test]
    fn env_file_loads_non_empty_values() {
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        path.push(format!("rss-ai-news-config-env-{unique}.env"));
        fs::write(
            &path,
            "OPENAI_API_KEY=sk-test\nOPENAI_BASE_URL=https://api.example.test/v1\nHTTP_PROXY=\n",
        )
        .expect("write temp env file");

        let config = load(Some(&path)).expect("env file loads");
        fs::remove_file(&path).expect("remove temp env file");

        assert_eq!(config.openai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            config.openai_base_url.as_deref(),
            Some("https://api.example.test/v1")
        );
        assert_eq!(config.http_proxy, None);
    }
}
