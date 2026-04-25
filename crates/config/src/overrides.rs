use std::path::PathBuf;

use crate::AppConfig;

#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub db_path: Option<PathBuf>,
    pub log_level: Option<String>,
    pub log_format: Option<String>,
    pub timezone: Option<String>,
    pub category_filter: Option<String>,
    pub dry_run: bool,
}

impl CliOverrides {
    pub fn apply_to_app(&self, app: &mut AppConfig) {
        if let Some(db_path) = &self.db_path {
            app.database.sqlite_path = db_path.clone();
        }
        if let Some(log_level) = &self.log_level {
            app.observability.log_level = log_level.clone();
        }
        if let Some(log_format) = &self.log_format {
            app.observability.log_format = log_format.clone();
        }
        if let Some(timezone) = &self.timezone {
            app.publish.target_timezone = timezone.clone();
        }
    }
}
