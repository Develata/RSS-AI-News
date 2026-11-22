use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

        // R1-P-S9 / R3F-S9: on Unix every temp file is created 0600, so no
        // other user can open it while the content is being written (a
        // descriptor opened then would keep reading after a later chmod).
        // Just before the rename it gets its final mode: the replaced
        // report's permissions, or for a new report the mode a plain create
        // in that directory gives (umask / default ACL), read from an empty
        // probe file. Windows: the temp file inherits the directory's ACL and
        // a replaced report's explicit ACL is not carried over (only the
        // read-only flag is), a documented limitation (plan/04 §4.1).
        let final_permissions = match existing_permissions(&dest).await? {
            Some(permissions) => Some(permissions),
            None => default_create_permissions(&dir).await?,
        };
        let temp = write_temp(&dir, report.markdown_content.as_bytes()).await?;
        if let Some(permissions) = final_permissions
            && let Err(error) = fs::set_permissions(&temp, permissions).await
        {
            remove_temp(&temp).await;
            return Err(error.into());
        }
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

/// Names tried before giving up on finding a free temp name.
const TEMP_NAME_ATTEMPTS: usize = 8;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A short hidden temp name, independent of the report's own name (a name
/// near the volume's 255-byte limit plus a suffix would not fit, R1-P-S8):
/// pid, clock and a process-wide counter, all hex.
fn next_temp_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(
        ".rss-ai-news.{:x}.{nanos:x}.{:x}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Permissions of the file `dest` currently names (following a link, as the
/// old in-place write did), `None` when there is none.
async fn existing_permissions(dest: &Path) -> Result<Option<std::fs::Permissions>, PublishError> {
    match fs::metadata(dest).await {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.permissions())),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Permissions a plain create in `dir` gives (0666 minus the umask, or what
/// a default ACL dictates), read from an empty probe file that is removed
/// again: std cannot read the umask, and calling `umask(2)` would race other
/// threads. The probe never holds report content. `None` off Unix.
#[cfg(unix)]
async fn default_create_permissions(
    dir: &Path,
) -> Result<Option<std::fs::Permissions>, PublishError> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let probe = dir.join(next_temp_name());
        let file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .await
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let permissions = file.metadata().await.map(|metadata| metadata.permissions());
        drop(file);
        remove_temp(&probe).await;
        return Ok(Some(permissions?));
    }
    Err(no_free_temp_name(dir))
}

#[cfg(not(unix))]
async fn default_create_permissions(
    _dir: &Path,
) -> Result<Option<std::fs::Permissions>, PublishError> {
    Ok(None)
}

fn no_free_temp_name(dir: &Path) -> PublishError {
    PublishError::LocalIoError(std::io::Error::new(
        ErrorKind::AlreadyExists,
        format!(
            "no free publish temp file name in {} after {TEMP_NAME_ATTEMPTS} attempts",
            dir.display()
        ),
    ))
}

/// Writes `contents` to a fresh hidden temp file in `dir` (same filesystem as
/// the destination, so the later rename is atomic) and fsyncs it. The
/// destination is untouched until [`commit_temp`]. On Unix the file is
/// created with mode 0600 (R3F-S9); the caller applies the final mode.
async fn write_temp(dir: &Path, contents: &[u8]) -> Result<PathBuf, PublishError> {
    write_temp_named(dir, contents, next_temp_name).await
}

/// [`write_temp`] with the name source injectable for tests. A name that
/// already exists belongs to someone else: it is never written to or removed,
/// another name is tried. Only a temp file this call created is removed when
/// writing it fails.
async fn write_temp_named(
    dir: &Path,
    contents: &[u8],
    mut next_name: impl FnMut() -> String,
) -> Result<PathBuf, PublishError> {
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let temp = dir.join(next_name());
        let mut options = fs::OpenOptions::new();
        // `create_new` (O_EXCL) never follows or reuses an existing path.
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = match options.open(&temp).await {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        // From here on the temp file is ours.
        let written = async {
            file.write_all(contents).await?;
            file.flush().await?;
            file.sync_all().await
        }
        .await;
        if let Err(error) = written {
            remove_temp(&temp).await;
            return Err(error.into());
        }
        return Ok(temp);
    }
    Err(no_free_temp_name(dir))
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

        let temp = write_temp(dir.path(), b"new report").await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old report");
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "new report");

        commit_temp(&temp, &dest, dir.path()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new report");
        assert!(!temp.exists());
    }

    /// R1-P-S8: a temp name that is already taken is someone else's file —
    /// left byte-for-byte alone, and another name is used.
    #[tokio::test]
    async fn taken_temp_name_is_neither_written_nor_removed() {
        let dir = tempfile::tempdir().unwrap();
        let taken = dir.path().join(".taken.tmp");
        std::fs::write(&taken, "not ours").unwrap();
        let mut names = vec![".fresh.tmp".to_string(), ".taken.tmp".to_string()];

        let temp = write_temp_named(dir.path(), b"report", || names.pop().unwrap())
            .await
            .unwrap();

        assert_eq!(temp, dir.path().join(".fresh.tmp"));
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "not ours");
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "report");
    }

    #[tokio::test]
    async fn every_temp_name_taken_is_an_error_that_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let taken = dir.path().join(".taken.tmp");
        std::fs::write(&taken, "not ours").unwrap();

        let error = write_temp_named(dir.path(), b"report", || ".taken.tmp".to_string())
            .await
            .unwrap_err();

        assert!(matches!(error, PublishError::LocalIoError(_)), "{error:?}");
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "not ours");
    }

    /// R3F-S9: the first create of a report (no file to replace yet) writes
    /// its content into a 0600 temp file too, whatever the umask; the one
    /// temp-file path serves replacements as well (R1-P-S9).
    #[cfg(unix)]
    #[tokio::test]
    async fn first_create_temp_file_is_created_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("new.md");
        let existing = existing_permissions(&dest).await.unwrap();
        assert!(existing.is_none());

        let temp = write_temp(dir.path(), b"report").await.unwrap();

        let mode = std::fs::metadata(&temp).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    #[test]
    fn temp_names_are_short_and_unique() {
        let first = next_temp_name();
        let second = next_temp_name();
        assert_ne!(first, second);
        assert!(first.len() < 64, "{first}");
        assert!(first.starts_with('.') && first.ends_with(".tmp"));
    }
}
