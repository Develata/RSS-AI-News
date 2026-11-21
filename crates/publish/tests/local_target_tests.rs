use std::path::Path;

use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_publish::{LocalFsTarget, PublishError, PublishTarget};

#[tokio::test]
async fn local_fs_target_writes_markdown_to_relative_path() {
    let dir = tempfile::tempdir().unwrap();
    let target = LocalFsTarget::new(dir.path().to_path_buf());
    let report = report("archive/ai/report.md", "# report\n");

    let artifact = target.publish(&report).await.unwrap();

    let local_path = artifact.local_path.expect("local path should be set");
    assert!(Path::new(&local_path).is_absolute());
    assert_eq!(
        tokio::fs::read_to_string(&local_path).await.unwrap(),
        "# report\n"
    );
    assert!(artifact.commit_sha.is_none());
    assert!(artifact.remote_target.is_none());
}

#[tokio::test]
async fn local_fs_target_creates_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let target = LocalFsTarget::new(dir.path().to_path_buf());
    let report = report("archive/ai/2025-01-15.md", "body");

    target.publish(&report).await.unwrap();

    assert!(dir.path().join("archive").join("ai").exists());
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("archive/ai/2025-01-15.md"))
            .await
            .unwrap(),
        "body"
    );
}

#[tokio::test]
async fn local_fs_target_rejects_path_with_parent_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let target = LocalFsTarget::new(dir.path().to_path_buf());
    let report = report("../escape.md", "body");

    let error = target.publish(&report).await.unwrap_err();

    assert!(matches!(error, PublishError::InvalidPath(path) if path == "../escape.md"));
}

/// R1-P-S8: the temp name does not embed the report's file name, so a
/// report whose own name is near the volume's 255-byte limit still publishes.
#[tokio::test]
async fn local_fs_target_publishes_a_report_whose_name_is_near_the_name_limit() {
    let dir = tempfile::tempdir().unwrap();
    let target = LocalFsTarget::new(dir.path().to_path_buf());
    let name = format!("{}.md", "a".repeat(240));
    let report = report(&format!("archive/{name}"), "long");

    target
        .publish(&report)
        .await
        .expect("long file name publishes");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("archive").join(&name)).unwrap(),
        "long"
    );
    let leftovers = std::fs::read_dir(dir.path().join("archive"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_name() != name.as_str())
        .count();
    assert_eq!(leftovers, 0, "no temp file is left behind");
}

/// Rename semantics: the new report replaces the directory entry instead of
/// truncating the old file in place, so an interrupted write can never leave a
/// half-written or empty report under the published name. A hard link to the
/// old inode keeps the old bytes only if the old file was never truncated.
#[cfg(unix)]
#[tokio::test]
async fn local_fs_target_replaces_existing_report_without_truncating_it_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("archive/ai/report.md");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&dest, "old report").unwrap();
    let old_inode = dir.path().join("old-inode");
    std::fs::hard_link(&dest, &old_inode).unwrap();
    let target = LocalFsTarget::new(dir.path().to_path_buf());

    target
        .publish(&report("archive/ai/report.md", "new report"))
        .await
        .unwrap();

    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new report");
    assert_eq!(std::fs::read_to_string(&old_inode).unwrap(), "old report");
    let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "report.md")
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_fs_target_rejects_parent_symlink_escaping_the_root() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("archive")).unwrap();
    let target = LocalFsTarget::new(root.path().to_path_buf());

    let error = target
        .publish(&report("archive/ai/report.md", "body"))
        .await
        .unwrap_err();

    assert!(matches!(error, PublishError::InvalidPath(_)), "{error:?}");
    assert!(
        std::fs::read_dir(outside.path()).unwrap().next().is_none(),
        "nothing may be created outside the output root"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_fs_target_rejects_destination_symlink_escaping_the_root() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim.md");
    std::fs::write(&victim, "untouched").unwrap();
    std::os::unix::fs::symlink(&victim, root.path().join("report.md")).unwrap();
    let target = LocalFsTarget::new(root.path().to_path_buf());

    let error = target
        .publish(&report("report.md", "body"))
        .await
        .unwrap_err();

    assert!(matches!(error, PublishError::InvalidPath(_)), "{error:?}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
}

#[cfg(unix)]
#[tokio::test]
async fn local_fs_target_follows_symlinks_that_stay_inside_the_root() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("real")).unwrap();
    std::os::unix::fs::symlink(root.path().join("real"), root.path().join("alias")).unwrap();
    let target = LocalFsTarget::new(root.path().to_path_buf());

    target
        .publish(&report("alias/report.md", "body"))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(root.path().join("real/report.md")).unwrap(),
        "body"
    );
}

fn report(relative_path: &str, markdown_content: &str) -> RenderedReport {
    RenderedReport {
        publish_record_id: 1,
        category_key: "ai".to_string(),
        report_date: "2026-04-28".to_string(),
        markdown_content: markdown_content.to_string(),
        relative_path: relative_path.to_string(),
    }
}
