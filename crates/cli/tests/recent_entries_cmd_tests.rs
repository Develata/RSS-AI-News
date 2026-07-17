use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rss_ai_news_cli::{
    args::{Cli, Command, LogFormat, OutputFormat, RecentEntriesArgs},
    commands::recent_entries,
    output::failure_envelope,
};
use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::OffsetDateTime;

#[tokio::test]
async fn recent_entries_read_path_does_not_mutate_database() {
    let fixture = Fixture::new(true).await;
    let before = directory_snapshot(fixture.config_dir.parent().expect("fixture root"));
    let cli = fixture.cli(false);
    let Command::RecentEntries(args) = &cli.command else {
        unreachable!()
    };

    let summary = recent_entries::run(&cli, args)
        .await
        .expect("recent-entries should succeed");

    assert_eq!(summary.category, "daily-math");
    assert_eq!(summary.entries.len(), 1);
    assert_eq!(summary.source_health.len(), 1);
    assert_only_sqlite_coordination_sidecars_changed(
        &before,
        &directory_snapshot(fixture.config_dir.parent().expect("fixture root")),
    );
}

#[tokio::test]
async fn recent_entries_dry_run_is_read_only_noop() {
    let fixture = Fixture::new(true).await;
    let before = directory_snapshot(fixture.config_dir.parent().expect("fixture root"));
    let cli = fixture.cli(true);
    let Command::RecentEntries(args) = &cli.command else {
        unreachable!()
    };

    let summary = recent_entries::run(&cli, args)
        .await
        .expect("--dry-run must remain a read-only successful query");

    assert_eq!(summary.entries.len(), 1);
    assert_only_sqlite_coordination_sidecars_changed(
        &before,
        &directory_snapshot(fixture.config_dir.parent().expect("fixture root")),
    );
}

#[tokio::test]
async fn recent_entries_fails_when_migration_pending() {
    let fixture = Fixture::new(true).await;
    let pool = build_sqlite_pool(&fixture.db_path, 1, 5_000)
        .await
        .expect("open fixture writer");
    let deleted = sqlx::query(
        "DELETE FROM _sqlx_migrations WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)",
    )
    .execute(&pool)
    .await
    .expect("mark newest migration pending")
    .rows_affected();
    assert_eq!(deleted, 1);
    pool.close().await;
    let cli = fixture.cli(false);
    let Command::RecentEntries(args) = &cli.command else {
        unreachable!()
    };

    let error = recent_entries::run(&cli, args)
        .await
        .expect_err("pending migration must fail closed");

    assert_eq!(error.command_name(), "recent-entries");
    assert_eq!(error.error_kind(), "recent_entries_migration_pending");
}

#[tokio::test]
async fn recent_entries_fails_closed_on_unknown_migration_version() {
    let fixture = Fixture::new(true).await;
    let pool = build_sqlite_pool(&fixture.db_path, 1, 5_000)
        .await
        .expect("open fixture writer");
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (999999, 'unknown', CURRENT_TIMESTAMP, 1, X'00', 0)",
    )
    .execute(&pool)
    .await
    .expect("insert unknown migration");
    pool.close().await;
    let cli = fixture.cli(false);
    let Command::RecentEntries(args) = &cli.command else {
        unreachable!()
    };

    let error = recent_entries::run(&cli, args)
        .await
        .expect_err("unknown migration must fail closed");

    assert_eq!(error.command_name(), "recent-entries");
    assert_eq!(error.error_kind(), "recent_entries_migration_drift");
    assert!(error.to_string().contains("migrate run"));
}

#[tokio::test]
async fn recent_entries_db_error_uses_recent_entries_json_envelope() {
    let fixture = Fixture::new(false).await;
    let cli = fixture.cli(false);
    let Command::RecentEntries(args) = &cli.command else {
        unreachable!()
    };

    let error = recent_entries::run(&cli, args)
        .await
        .expect_err("missing read-only DB must fail");
    let envelope = failure_envelope(error.command_name(), &error);

    assert_eq!(error.command_name(), "recent-entries");
    assert_eq!(envelope["command"], "recent-entries");
    assert_eq!(envelope["status"], "error");
    assert!(envelope["summary"].is_null());
    assert_eq!(envelope["errors"].as_array().map(Vec::len), Some(1));
    assert!(
        !fixture.db_path.exists(),
        "read path must not create missing DB"
    );
}

struct Fixture {
    _temp: TempDir,
    config_dir: PathBuf,
    db_path: PathBuf,
}

impl Fixture {
    async fn new(seed_database: bool) -> Self {
        let temp = TempDir::new().expect("temp dir");
        let config_dir = temp.path().join("config");
        let db_path = temp.path().join("rss.sqlite");
        write_config(&config_dir, &db_path);
        if seed_database {
            seed_database_file(&db_path).await;
        }
        Self {
            _temp: temp,
            config_dir,
            db_path,
        }
    }

    fn cli(&self, dry_run: bool) -> Cli {
        let args = RecentEntriesArgs {
            discovered_after: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            limit: 50,
        };
        Cli {
            config_dir: self.config_dir.clone(),
            db_path: Some(self.db_path.clone()),
            log_level: "info".to_string(),
            log_format: LogFormat::Pretty,
            log_file: String::new(),
            metrics_bind: String::new(),
            output_format: OutputFormat::Json,
            dry_run,
            category: Some("daily-math".to_string()),
            timezone: None,
            command: Command::RecentEntries(args),
        }
    }
}

async fn seed_database_file(path: &Path) {
    let pool = build_sqlite_pool(path, 1, 5_000)
        .await
        .expect("create fixture DB");
    run_migrations(&StoragePool::Sqlite(pool.clone()))
        .await
        .expect("migrate fixture DB");
    let config: i64 = sqlx::query_scalar(
        "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) \
         VALUES ('config', 'recent-fixture', 'fixture', 'fixture-sha') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert config rule");
    let source: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO feed_sources (
            category_key, source_key, display_name, feed_url, feed_kind,
            status, priority, config_version
        )
        VALUES ('daily-math', 'fixture-source', 'Fixture Source',
                'https://example.test/feed.xml', 'rss', 'active', 10, ?)
        RETURNING id
        "#,
    )
    .bind(config)
    .fetch_one(&pool)
    .await
    .expect("insert source");
    sqlx::query(
        r#"
        INSERT INTO feed_entries (
            source_id, feed_entry_uid, normalized_link, link_hash, title_raw,
            summary_raw, discovered_at, state, dedup_decision
        )
        VALUES (?, 'fixture-entry', 'https://example.test/post', 'fixture-hash',
                'Fixture title', 'sensitive body not projected', ?, 'pending_fetch', 'fresh')
        "#,
    )
    .bind(source)
    .bind(OffsetDateTime::from_unix_timestamp(100).unwrap())
    .execute(&pool)
    .await
    .expect("insert entry");
    pool.close().await;
}

fn write_config(root: &Path, db_path: &Path) {
    fs::create_dir_all(root.join("categories")).expect("create config dir");
    let db_path = db_path.to_string_lossy().replace('\\', "/");
    let app = include_str!("../../../configs/app.toml.example")
        .replace("data/rss-ai-news.db", &db_path)
        .replace("enabled = true", "enabled = false");
    fs::write(root.join("app.toml"), app).expect("write app.toml");
    fs::write(
        root.join("categories/daily-math.toml"),
        r#"
schema_version = "1"

[category]
key = "daily-math"
display_name = "Daily Mathematics"
priority = 10

[[sources]]
key = "fixture-source"
display_name = "Fixture Source"
feed_url = "https://example.test/feed.xml"
feed_kind = "rss"
priority = 10
enabled = true
"#,
    )
    .expect("write category");
}

fn assert_only_sqlite_coordination_sidecars_changed(
    before: &BTreeMap<PathBuf, String>,
    after: &BTreeMap<PathBuf, String>,
) {
    let is_sidecar = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("-wal") || name.ends_with("-shm"))
    };

    for (path, digest) in before {
        if !is_sidecar(path) {
            assert_eq!(
                after.get(path),
                Some(digest),
                "non-SQLite-sidecar file changed or disappeared: {}",
                path.display()
            );
        }
    }
    for (path, digest) in after {
        if !before.contains_key(path) {
            assert!(
                is_sidecar(path),
                "read-only command created unexpected file: {}",
                path.display()
            );
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("-wal"))
            {
                assert_eq!(
                    digest,
                    &format!("{:x}", Sha256::digest([])),
                    "new read-only WAL sidecar must be empty"
                );
            }
        }
    }
}

fn directory_snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    fn visit(root: &Path, current: &Path, snapshot: &mut BTreeMap<PathBuf, String>) {
        for entry in fs::read_dir(current).expect("read fixture directory") {
            let path = entry.expect("read fixture entry").path();
            if path.is_dir() {
                visit(root, &path, snapshot);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("fixture path under root")
                    .to_path_buf();
                let bytes = fs::read(&path).expect("read fixture file for digest");
                snapshot.insert(relative, format!("{:x}", Sha256::digest(bytes)));
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}
