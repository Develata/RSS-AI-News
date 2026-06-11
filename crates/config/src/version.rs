use sha2::{Digest, Sha256};

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

// W16（docs/plan/16-config-versioning.md §5）：原 `ConfigVersionStore` trait
// 及其错误类型已删除——生产路径零调用，且作为 config 行的第二写入口违反
// 单一真相源。config 行的注册/轮换统一走 storage 层 `rotate_active_config`。

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
