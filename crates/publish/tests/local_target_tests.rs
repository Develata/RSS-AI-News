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

fn report(relative_path: &str, markdown_content: &str) -> RenderedReport {
    RenderedReport {
        publish_record_id: 1,
        category_key: "ai".to_string(),
        report_date: "2026-04-28".to_string(),
        markdown_content: markdown_content.to_string(),
        relative_path: relative_path.to_string(),
    }
}
