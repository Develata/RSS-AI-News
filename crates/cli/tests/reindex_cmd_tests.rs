use std::fs;

use clap::Parser;
use rss_ai_news_cli::{
    args::{Cli, Command},
    commands::reindex::{self, ReindexMode},
};
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use tempfile::TempDir;

#[tokio::test]
async fn reindex_dry_run_preserves_config_and_all_database_tables() {
    let (_temp, cli) = fixture();
    let pool = build_sqlite_pool(cli.db_path.as_ref().unwrap(), 1, 5_000)
        .await
        .unwrap();
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) \
         VALUES ('config', 'previous', 'previous config', 'previous-sha')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let before: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    let Command::Reindex(args) = &cli.command else {
        unreachable!()
    };

    let summary = reindex::run(&cli, args).await.unwrap();

    assert_eq!(summary.mode, ReindexMode::DryRun);
    assert_eq!(summary.per_target.len(), 3);
    let after: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "dry-run must not commit any database changes"
    );
    let active: String = sqlx::query_scalar(
        "SELECT payload_sha256 FROM rule_versions WHERE kind = 'config' AND status = 'active'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, "previous-sha");
    pool.close().await;
}

#[tokio::test]
async fn reindex_dry_run_does_not_create_missing_database() {
    let (_temp, cli) = fixture();
    let Command::Reindex(args) = &cli.command else {
        unreachable!()
    };
    assert!(reindex::run(&cli, args).await.is_err());
    assert!(!cli.db_path.as_ref().unwrap().exists());
}

#[tokio::test]
async fn reindex_dry_run_does_not_apply_pending_migrations() {
    let (_temp, cli) = fixture();
    let pool = build_sqlite_pool(cli.db_path.as_ref().unwrap(), 1, 5_000)
        .await
        .unwrap();
    let Command::Reindex(args) = &cli.command else {
        unreachable!()
    };
    assert!(reindex::run(&cli, args).await.is_err());
    let table_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(table_count, 0, "dry-run must not migrate an empty database");
    pool.close().await;
}

#[tokio::test]
async fn global_dry_run_does_not_create_database_for_reindex() {
    let (_temp, mut cli) = fixture();
    cli.dry_run = true;
    let Command::Reindex(args) = &mut cli.command else {
        unreachable!()
    };
    args.dry_run = false;
    let Command::Reindex(args) = &cli.command else {
        unreachable!()
    };
    assert!(reindex::run(&cli, args).await.is_err());
    assert!(!cli.db_path.as_ref().unwrap().exists());
}

#[tokio::test]
async fn reindex_abort_with_dry_run_is_rejected_before_opening_storage() {
    let (_temp, mut cli) = fixture();
    let Command::Reindex(args) = &mut cli.command else {
        unreachable!()
    };
    args.target = None;
    args.abort = Some("1".to_owned());
    let Command::Reindex(args) = &cli.command else {
        unreachable!()
    };
    let error = reindex::run(&cli, args).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot be combined with --dry-run")
    );
    assert!(!cli.db_path.as_ref().unwrap().exists());
}

#[tokio::test]
async fn reindex_category_filter_cannot_archive_unselected_categories() {
    let (_temp, mut cli) = fixture();
    let pool = build_sqlite_pool(cli.db_path.as_ref().unwrap(), 1, 5_000)
        .await
        .unwrap();
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .unwrap();
    sqlx::query("INSERT INTO rule_versions(kind, version_tag, description, payload_sha256) VALUES('config', 'seed', 'seed', 'seed')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO feed_sources(category_key, source_key, display_name, feed_url, feed_kind, config_version) VALUES('other', 'fixture', 'Other', 'https://example.test/other.xml', 'rss', 1)")
        .execute(&pool).await.unwrap();
    fs::write(
        cli.config_dir.join("categories/other.toml"),
        fs::read_to_string(cli.config_dir.join("categories/test.toml"))
            .unwrap()
            .replace("key = \"test\"", "key = \"other\""),
    )
    .unwrap();
    cli.category = Some("test".into());
    let Command::Reindex(args) = &mut cli.command else {
        unreachable!()
    };
    args.target = Some(rss_ai_news_cli::args::ReindexTarget::Categories);
    args.dry_run = false;
    let args = args.clone();
    let result = reindex::run(&cli, &args).await;
    let state: String =
        sqlx::query_scalar("SELECT status FROM feed_sources WHERE category_key = 'other'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        state, "active",
        "a category filter must never archive unrelated sources"
    );
    let error = result.expect_err("reindex upgrades global rules and must reject category filters");
    assert_eq!(error.exit_code().as_i32(), 2);
    assert!(error.display_user().contains("--category"));
    pool.close().await;
}

#[tokio::test]
async fn reindex_rejects_filtered_global_targets_before_opening_storage() {
    let (_temp, mut cli) = fixture();
    cli.category = Some("test".into());
    for target in [
        rss_ai_news_cli::args::ReindexTarget::LinkHash,
        rss_ai_news_cli::args::ReindexTarget::ContentHash,
        rss_ai_news_cli::args::ReindexTarget::All,
    ] {
        let Command::Reindex(args) = &mut cli.command else {
            unreachable!()
        };
        args.target = Some(target);
        let args = args.clone();
        let error = reindex::run(&cli, &args)
            .await
            .expect_err("global reindex cannot be filtered");
        assert_eq!(error.exit_code().as_i32(), 2);
        assert!(error.display_user().contains("--category"));
        assert!(!cli.db_path.as_ref().unwrap().exists());
    }
}

fn fixture() -> (TempDir, Cli) {
    let temp = TempDir::new().unwrap();
    let config_dir = temp.path().join("config");
    let db_path = temp.path().join("rss.sqlite");
    fs::create_dir_all(config_dir.join("categories")).unwrap();
    fs::write(
        config_dir.join("app.toml"),
        include_str!("../../../configs/app.toml.example")
            .replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    fs::write(
        config_dir.join("categories/test.toml"),
        r#"
schema_version = "1"
[category]
key = "test"
display_name = "Test"
priority = 10
[[sources]]
key = "fixture"
display_name = "Fixture"
feed_url = "https://example.test/feed.xml"
feed_kind = "rss"
priority = 10
enabled = true
"#,
    )
    .unwrap();
    let cli = Cli::try_parse_from([
        "rss-ai-news",
        "--config-dir",
        config_dir.to_str().unwrap(),
        "--db-path",
        db_path.to_str().unwrap(),
        "reindex",
        "--target",
        "all",
        "--dry-run",
    ])
    .unwrap();
    (temp, cli)
}
