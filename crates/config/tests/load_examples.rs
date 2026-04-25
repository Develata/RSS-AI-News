use std::{fs, path::PathBuf, time::SystemTime};

use rss_ai_news_config::{CliOverrides, load};

#[test]
fn load_example_configs_end_to_end() {
    let root = temp_root();
    let config_dir = root.join("configs");
    let categories_dir = config_dir.join("categories");
    fs::create_dir_all(&categories_dir).expect("create temp config dirs");

    fs::write(
        config_dir.join("app.toml"),
        include_str!("../../../configs/app.toml.example"),
    )
    .expect("write app fixture");
    fs::write(
        categories_dir.join("ai.toml"),
        include_str!("../../../configs/categories/ai.toml.example"),
    )
    .expect("write category fixture");
    let env_file = root.join(".env");
    fs::write(
        &env_file,
        "OPENAI_API_KEY=sk-test\nOPENAI_BASE_URL=https://api.example.test/v1\nRSSHUB_BASE_URL=https://rsshub.example.test\n",
    )
    .expect("write env fixture");

    let config =
        load(&config_dir, Some(&env_file), CliOverrides::default()).expect("example config loads");
    let effective = config
        .effective_for_category("ai")
        .expect("effective config for ai category");

    assert_eq!(config.categories.len(), 1);
    assert_eq!(config.config_sha256.len(), 64);
    assert!(effective.ai_enabled);
    assert!(!effective.include_unscored);
    assert_eq!(effective.max_items_per_report, 30);
    assert_eq!(effective.min_importance_score, 30);
    assert_eq!(effective.model, "gpt-4o-mini");
    assert_eq!(effective.max_input_chars, 10000);

    fs::remove_dir_all(root).expect("cleanup temp config dir");
}

fn temp_root() -> PathBuf {
    let mut path = std::env::temp_dir();
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    path.push(format!("rss-ai-news-config-load-{unique}"));
    path
}
