//! W11-P3-A.4：双方言 migration 编号配对检查。
//!
//! 锁定不变量：`migrations/sqlite/` 与 `migrations/postgres/` 两侧目录的
//! `NNNN_<name>.up.sql` / `NNNN_<name>.down.sql` 一一对应（含编号 NNNN 与
//! base name），任一侧缺失即编译期 fail，避免方言落后于 schema 演进。
//!
//! `sqlx::migrate!` 在编译期会展开宏并把指定目录的 SQL 文件嵌入二进制；
//! 单跑 SQLite 套件时不会主动校验 Postgres 侧存在性，本测试是 CI 第一道关。

use std::{collections::BTreeMap, fs, path::PathBuf};

/// 把 `migrations/<dialect>/` 目录里所有 `NNNN_*.up.sql` / `*.down.sql` 解析成
/// `{ NNNN: (base_name, has_up, has_down) }`，便于两侧 diff。
#[derive(Debug, Default, PartialEq, Eq)]
struct DialectMigrations {
    /// key 是 NNNN（四位编号）；value.0 是 base name（不含扩展），value.1/.2 是
    /// 是否各有 up/down 文件。
    entries: BTreeMap<String, (String, bool, bool)>,
}

fn read_dialect(dir: &PathBuf) -> DialectMigrations {
    let mut entries: BTreeMap<String, (String, bool, bool)> = BTreeMap::new();
    let read_dir = fs::read_dir(dir).unwrap_or_else(|e| {
        panic!("read migrations dir {dir:?}: {e}");
    });
    for entry in read_dir {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // 期望形如 `0001_init.up.sql` / `0002_xxx.down.sql`
        let Some((stem, suffix)) = split_suffix(filename) else {
            continue;
        };
        let Some((nnnn, base)) = stem.split_once('_') else {
            panic!("migration filename without NNNN_ prefix: {filename}");
        };
        assert_eq!(
            nnnn.len(),
            4,
            "migration NNNN must be 4 digits, got `{nnnn}` in {filename}"
        );
        assert!(
            nnnn.chars().all(|c| c.is_ascii_digit()),
            "migration NNNN must be all digits, got `{nnnn}` in {filename}"
        );
        let slot = entries
            .entry(nnnn.to_string())
            .or_insert_with(|| (base.to_string(), false, false));
        assert_eq!(
            slot.0, base,
            "migration {nnnn} has divergent base name: {} vs {base}",
            slot.0
        );
        match suffix {
            "up" => slot.1 = true,
            "down" => slot.2 = true,
            _ => unreachable!("split_suffix returned unexpected suffix: {suffix}"),
        }
    }
    DialectMigrations { entries }
}

/// 文件名是 `<stem>.<suffix>.sql` 形式时返回 `(stem, suffix)`，否则 None。
fn split_suffix(filename: &str) -> Option<(&str, &str)> {
    let stripped = filename.strip_suffix(".sql")?;
    let (stem, suffix) = stripped.rsplit_once('.')?;
    if suffix != "up" && suffix != "down" {
        return None;
    }
    Some((stem, suffix))
}

fn workspace_migrations_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/storage；回退两级到仓库根，再进 migrations
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("migrations")
}

#[test]
fn migrations_sqlite_and_postgres_have_matching_numbers_and_basenames() {
    let root = workspace_migrations_root();
    let sqlite = read_dialect(&root.join("sqlite"));
    let postgres = read_dialect(&root.join("postgres"));

    let sqlite_keys: Vec<&String> = sqlite.entries.keys().collect();
    let postgres_keys: Vec<&String> = postgres.entries.keys().collect();
    assert_eq!(
        sqlite_keys, postgres_keys,
        "migration NNNN sets diverge: sqlite={sqlite_keys:?}, postgres={postgres_keys:?}"
    );

    for (nnnn, (sqlite_base, sqlite_up, sqlite_down)) in &sqlite.entries {
        let (postgres_base, postgres_up, postgres_down) = postgres
            .entries
            .get(nnnn)
            .expect("checked equal keysets above");
        assert_eq!(
            sqlite_base, postgres_base,
            "migration {nnnn} base name differs: sqlite=`{sqlite_base}`, postgres=`{postgres_base}`"
        );
        assert!(
            *sqlite_up && *postgres_up,
            "migration {nnnn} missing .up.sql: sqlite={sqlite_up}, postgres={postgres_up}"
        );
        assert!(
            *sqlite_down && *postgres_down,
            "migration {nnnn} missing .down.sql: sqlite={sqlite_down}, postgres={postgres_down}"
        );
    }
}
