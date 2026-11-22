//! W17：已应用迁移文件的不可变锁定。
//!
//! `sqlx::migrate!` 嵌入迁移内容并在 `Migrator::run` 时对**已应用**迁移做
//! 校验和比对——文件字节哪怕只改一个注释字符，存量库下次启动就会报
//! `VersionMismatch` 直接拒绝。W17 险些因为给
//! `migrations/postgres/0001_init.up.sql` 改文档路径注释打挂生产 PG 库
//! （275fecd 引入，随本测试一并回滚），故用内容哈希把已发布的迁移钉死。
//!
//! 维护规则：
//! - **新增**迁移（0004+）：在 `PINNED_UP_MIGRATIONS` 追加一行（哈希用
//!   `tr -d '\r' < file | sha256sum` 计算），两方言都要。
//! - **修改/删除**已钉住的文件：禁止。需要修 schema 就加新编号迁移。
//! - 哈希对 CR 归一化（剥 `\r`），避免 Windows autocrlf 工作树与 CI 的
//!   LF 检出算出不同值。

use sha2::{Digest, Sha256};

/// (相对仓库根的路径, CR 归一化内容的 SHA-256 hex)
const PINNED_UP_MIGRATIONS: &[(&str, &str)] = &[
    (
        "migrations/sqlite/0001_init.up.sql",
        "be42114a004c8cb077a6702bcf4bd4e0a36d71c2d6cd1475dce3953aa23be2ac",
    ),
    (
        "migrations/sqlite/0002_reindex_jobs_and_rule_status.up.sql",
        "6cde86fd3676ffa1f92789765deda4e73acccf1946ef4d94138d5c20f1c0dd56",
    ),
    (
        "migrations/sqlite/0003_ai_effective_model_id.up.sql",
        "7f919c78bf78a97ae8396ae5c26f6c8a7197ab5b0fd9098da2cca18fbe367f6b",
    ),
    (
        "migrations/sqlite/0004_feed_entry_link_dedup_atomicity.up.sql",
        "bd3974a593eb6d72508f679d040952279a4d1b8f39cc059860f35d5aed054dc4",
    ),
    (
        "migrations/postgres/0001_init.up.sql",
        "f4376d3b88134b090de3634a824a1934f691aab37c3971b6b6247010869d40ef",
    ),
    (
        "migrations/postgres/0002_reindex_jobs_and_rule_status.up.sql",
        "0f356fd14cfaad041c4eb533a8a7c7007d7111842e8b69fb35c4c0a59d1fa155",
    ),
    (
        "migrations/postgres/0003_ai_effective_model_id.up.sql",
        "7f919c78bf78a97ae8396ae5c26f6c8a7197ab5b0fd9098da2cca18fbe367f6b",
    ),
    (
        "migrations/postgres/0004_feed_entry_link_dedup_atomicity.up.sql",
        "77aa6e7d980ca444b5523712f84d35041e78f56d3b898daf2878b4ae23a08c30",
    ),
    (
        "migrations/sqlite/0005_artifact_reference_indexes.up.sql",
        "3c9e9cb9e5289adba5f4d21084a2c61a821d3998c8336c176122d455170efa0b",
    ),
    (
        "migrations/postgres/0005_artifact_reference_indexes.up.sql",
        "3c9e9cb9e5289adba5f4d21084a2c61a821d3998c8336c176122d455170efa0b",
    ),
    (
        "migrations/sqlite/0006_decision_results.up.sql",
        "63f6fd1fed1c40c1ccb47260d54ae959374459257aea14b34503f7076b963f63",
    ),
    (
        "migrations/postgres/0006_decision_results.up.sql",
        "a457b04b66c477897e849b269cefc39dc4477d0af3b0b09f53a38db82f93fddb",
    ),
    (
        "migrations/sqlite/0007_feed_entry_retry_cooldown.up.sql",
        "954b1b808959df65d456faa1fe2d044abe0d34ec06e82104e1b0d1176fc0c9d6",
    ),
    (
        "migrations/postgres/0007_feed_entry_retry_cooldown.up.sql",
        "a53362425f3bb21a1e44bd83e217d977776b1d59f42c9c7ee69f4ed89ef17c56",
    ),
    (
        "migrations/sqlite/0008_publish_stage_budget_and_item_article_index.up.sql",
        // Amended before release for R1-P-S12 (publish_records.next_attempt_at).
        "7608efb6dc0eb3439dc7b7aaa3b9c99cffb40e2a70fdf19250adffb841a23fca",
    ),
    (
        "migrations/postgres/0008_publish_stage_budget_and_item_article_index.up.sql",
        "10f594c3577e7ed4ad3c53d6bf71f4fce30fe2b99a10d7a0ecfd943fd1a137d3",
    ),
    (
        "migrations/sqlite/0009_ai_share_state_and_article_origin_index.up.sql",
        "4f2f16c7736090abdc967578a9dcb259bdecb75978224d4e3fc6b9b10dc550c1",
    ),
    (
        "migrations/postgres/0009_ai_share_state_and_article_origin_index.up.sql",
        "0016bedd1799839984d22584afe2b6d5ad667b1464302f4e6dca100d30d77605",
    ),
    (
        "migrations/sqlite/0010_publish_unverified_write_blob.up.sql",
        "9db59f04aa63f71930a748f2179f2b99d113cbaed292bf4bebdefea4ab43c0dd",
    ),
    (
        "migrations/postgres/0010_publish_unverified_write_blob.up.sql",
        "9db59f04aa63f71930a748f2179f2b99d113cbaed292bf4bebdefea4ab43c0dd",
    ),
];

fn repo_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR = crates/storage；迁移目录在仓库根（与
    // src/migrate.rs 里 `sqlx::migrate!("../../migrations/...")` 同基准）。
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../")
}

fn normalized_sha256(path: &std::path::Path) -> String {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("pinned migration {} must be readable: {e}", path.display()));
    let normalized = raw.replace('\r', "");
    let digest = Sha256::digest(normalized.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 每个已钉住的 up 迁移文件内容必须逐字节（CR 归一化后）不变。
#[test]
fn pinned_up_migrations_are_immutable() {
    let root = repo_root();
    for (rel, expected) in PINNED_UP_MIGRATIONS {
        let actual = normalized_sha256(&root.join(rel));
        assert_eq!(
            &actual, expected,
            "{rel} 内容已变化。已应用的迁移文件不可修改（sqlx 校验和会让\
             存量库启动报 VersionMismatch）；要改 schema 请新增编号迁移。\
             若这是全新未发布的迁移，请更新 PINNED_UP_MIGRATIONS 中的哈希。"
        );
    }
}

/// 迁移目录里的 up 文件集合必须与钉住清单一致：
/// - 新增迁移未登记 → 提醒追加 pin 行
/// - 已钉住文件被删除/改名 → 拒绝
#[test]
fn up_migration_file_set_matches_pin_list() {
    let root = repo_root();
    for dialect in ["sqlite", "postgres"] {
        let dir = root.join("migrations").join(dialect);
        let mut found: Vec<String> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("migrations/{dialect} must exist: {e}"))
            .filter_map(|entry| {
                let name = entry.expect("dir entry").file_name();
                let name = name.to_string_lossy().into_owned();
                name.ends_with(".up.sql")
                    .then(|| format!("migrations/{dialect}/{name}"))
            })
            .collect();
        found.sort();

        let mut pinned: Vec<String> = PINNED_UP_MIGRATIONS
            .iter()
            .map(|(rel, _)| (*rel).to_string())
            .filter(|rel| rel.starts_with(&format!("migrations/{dialect}/")))
            .collect();
        pinned.sort();

        assert_eq!(
            found, pinned,
            "migrations/{dialect} 的 up 文件集合与 PINNED_UP_MIGRATIONS 不一致：\
             新增迁移请追加 pin 行；已钉住的文件不可删除/改名。"
        );
    }
}
