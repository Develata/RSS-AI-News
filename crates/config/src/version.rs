use sha2::{Digest, Sha256};
use thiserror::Error;

pub fn compute_config_sha256(
    app_toml_content: &str,
    category_toml_contents: &[(String, String)],
) -> String {
    let mut categories = category_toml_contents.to_vec();
    categories.sort_by(|left, right| left.0.cmp(&right.0));

    let mut input = String::new();
    input.push_str("app.toml::");
    input.push_str(app_toml_content);
    input.push('\n');
    for (name, content) in categories {
        input.push_str("categories/");
        input.push_str(&name);
        input.push_str("::");
        input.push_str(&content);
        input.push('\n');
    }

    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub trait ConfigVersionStore {
    /// Returns the rule_versions.id for the given config sha256.
    /// Inserts a new row if not found.
    fn get_or_create_config_version(&self, sha256: &str) -> Result<i64, ConfigVersionStoreError>;
}

#[derive(Debug, Error)]
pub enum ConfigVersionStoreError {
    #[error("config version store error: {0}")]
    Storage(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_input_produces_same_sha256() {
        let left = compute_config_sha256("app", &[("ai.toml".to_string(), "category".to_string())]);
        let right =
            compute_config_sha256("app", &[("ai.toml".to_string(), "category".to_string())]);

        assert_eq!(left, right);
        assert_eq!(left.len(), 64);
    }

    #[test]
    fn different_input_changes_sha256() {
        let left = compute_config_sha256("app", &[("ai.toml".to_string(), "category".to_string())]);
        let right =
            compute_config_sha256("app2", &[("ai.toml".to_string(), "category".to_string())]);

        assert_ne!(left, right);
    }

    #[test]
    fn category_filename_order_does_not_affect_sha256() {
        let left = compute_config_sha256(
            "app",
            &[
                ("b.toml".to_string(), "b".to_string()),
                ("a.toml".to_string(), "a".to_string()),
            ],
        );
        let right = compute_config_sha256(
            "app",
            &[
                ("a.toml".to_string(), "a".to_string()),
                ("b.toml".to_string(), "b".to_string()),
            ],
        );

        assert_eq!(left, right);
    }
}
