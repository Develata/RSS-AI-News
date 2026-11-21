use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rss_ai_news_domain::dto::publish::RenderedReport;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::error::PublishError;
use crate::target::{PublishTarget, PublishedArtifact};

#[derive(Debug, Clone)]
pub struct LocalFsTarget {
    base_dir: PathBuf,
}

impl LocalFsTarget {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }
}

#[async_trait]
impl PublishTarget for LocalFsTarget {
    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        let relative = Path::new(&report.relative_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(PublishError::InvalidPath(report.relative_path.clone()));
        }
        let file_name = relative
            .file_name()
            .ok_or_else(|| PublishError::InvalidPath(report.relative_path.clone()))?;

        // The output root itself is operator-configured and may be a symlink;
        // everything below it must stay inside its canonical location.
        fs::create_dir_all(&self.base_dir).await?;
        let root = fs::canonicalize(&self.base_dir).await?;
        let parent = relative.parent().unwrap_or(Path::new(""));
        let dir = resolve_dir_under_root(&root, parent, &report.relative_path).await?;
        let dest = dir.join(file_name);
        reject_escaping_symlink(&root, &dest, &report.relative_path).await?;

        let temp = write_temp(&dir, file_name, report.markdown_content.as_bytes()).await?;
        commit_temp(&temp, &dest, &dir).await?;

        // Reported path keeps the configured (non-canonical) spelling.
        let absolute = self.base_dir.join(relative);
        Ok(PublishedArtifact {
            local_path: Some(absolute.to_string_lossy().into_owned()),
            commit_sha: None,
            remote_target: None,
        })
    }
}

fn escape(relative_path: &str) -> PublishError {
    PublishError::InvalidPath(format!(
        "{relative_path}: resolves outside the publish output root"
    ))
}

/// Walks `parent` one component at a time below the canonical `root`,
/// creating missing directories. An existing symlink is followed only if its
/// canonical target stays under `root`; `create_dir_all` is avoided because it
/// would follow an escaping link and create directories outside the root
/// before any check could run. Returns the canonical directory.
async fn resolve_dir_under_root(
    root: &Path,
    parent: &Path,
    relative_path: &str,
) -> Result<PathBuf, PublishError> {
    let mut current = root.to_path_buf();
    for component in parent.components() {
        let name = match component {
            Component::CurDir => continue,
            Component::Normal(name) => name,
            _ => return Err(PublishError::InvalidPath(relative_path.to_string())),
        };
        let candidate = current.join(name);
        let metadata = match fs::symlink_metadata(&candidate).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                match fs::create_dir(&candidate).await {
                    Ok(()) => {}
                    // Raced with another writer; re-inspect what is there now.
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                fs::symlink_metadata(&candidate).await?
            }
            Err(error) => return Err(error.into()),
        };
        current = if metadata.file_type().is_symlink() {
            let resolved = fs::canonicalize(&candidate)
                .await
                .map_err(|_| escape(relative_path))?;
            if !resolved.starts_with(root) {
                return Err(escape(relative_path));
            }
            resolved
        } else {
            candidate
        };
        if !fs::metadata(&current).await?.is_dir() {
            return Err(PublishError::InvalidPath(format!(
                "{relative_path}: {} is not a directory",
                current.display()
            )));
        }
    }
    Ok(current)
}

/// The rename below replaces a destination symlink rather than writing through
/// it, but a link pointing outside the root still signals tampering with the
/// output tree, so it is refused (a dangling link likewise).
async fn reject_escaping_symlink(
    root: &Path,
    dest: &Path,
    relative_path: &str,
) -> Result<(), PublishError> {
    match fs::symlink_metadata(dest).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let resolved = fs::canonicalize(dest)
                .await
                .map_err(|_| escape(relative_path))?;
            if resolved.starts_with(root) {
                Ok(())
            } else {
                Err(escape(relative_path))
            }
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Writes `contents` to a fresh hidden temp file in `dir` (same filesystem as
/// the destination, so the later rename is atomic) and fsyncs it. The
/// destination is untouched until [`commit_temp`].
async fn write_temp(
    dir: &Path,
    file_name: &std::ffi::OsStr,
    contents: &[u8],
) -> Result<PathBuf, PublishError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let temp = dir.join(format!(
        ".{}.{}.{nanos}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let written = async {
        // `create_new` (O_EXCL) never follows or reuses an existing path.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        file.write_all(contents).await?;
        file.flush().await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = written {
        remove_temp(&temp).await;
        return Err(error.into());
    }
    Ok(temp)
}

/// Atomically replaces `dest` with `temp`, then fsyncs the directory so the
/// rename itself survives a crash (Unix; best effort).
async fn commit_temp(temp: &Path, dest: &Path, dir: &Path) -> Result<(), PublishError> {
    if let Err(error) = fs::rename(temp, dest).await {
        remove_temp(temp).await;
        return Err(error.into());
    }
    #[cfg(unix)]
    {
        let synced = async { fs::File::open(dir).await?.sync_all().await }.await;
        if let Err(error) = synced {
            // The report is already in place; only rename durability is at
            // stake, which is not worth failing the publish over.
            tracing::warn!(dir = %dir.display(), %error, "fsync of publish directory failed");
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

async fn remove_temp(temp: &Path) {
    if let Err(error) = fs::remove_file(temp).await
        && error.kind() != ErrorKind::NotFound
    {
        tracing::warn!(temp = %temp.display(), %error, "failed to remove publish temp file");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An interruption after the temp write but before the commit (crash,
    /// cancelled future) leaves the published report byte-for-byte intact.
    #[tokio::test]
    async fn interrupted_write_before_commit_leaves_old_report_intact() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("report.md");
        std::fs::write(&dest, "old report").unwrap();

        let temp = write_temp(dir.path(), dest.file_name().unwrap(), b"new report")
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old report");
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "new report");

        commit_temp(&temp, &dest, dir.path()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new report");
        assert!(!temp.exists());
    }
}
