use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

pub(crate) fn assert_published_after(stdout: &str, expected: Option<&str>) -> Result<(), String> {
    let envelope: Value = serde_json::from_str(stdout)
        .map_err(|error| format!("recent-entries stdout is not JSON: {error}"))?;
    if envelope.get("status").and_then(Value::as_str) != Some("success") {
        return Err(format!("unexpected command status: {}", envelope["status"]));
    }
    let actual = envelope
        .pointer("/summary/published_after")
        .ok_or_else(|| "summary.published_after is missing".to_string())?;
    match expected {
        None if actual.is_null() => Ok(()),
        Some(expected) if actual.as_str() == Some(expected) => Ok(()),
        _ => Err(format!(
            "summary.published_after = {actual}, expected {:?}",
            expected
        )),
    }
}

pub(crate) fn check_tooling_dependency_boundary(repo_root: &Path) -> Result<(), String> {
    let manifest = fs::read_to_string(repo_root.join("tools/acceptance/Cargo.toml"))
        .map_err(|error| format!("cannot read acceptance manifest: {error}"))?;
    let dependencies = manifest
        .split("[dependencies]")
        .nth(1)
        .and_then(|tail| tail.split("\n[").next())
        .ok_or_else(|| "acceptance manifest has no [dependencies] section".to_string())?;
    let product_dependencies = dependencies
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("rss-ai-news-"))
        .collect::<Vec<_>>();
    if product_dependencies.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "acceptance tooling must not depend on product crates: {product_dependencies:?}"
        ))
    }
}

pub(crate) fn check_acceptance_documents(repo_root: &Path) -> Result<(), String> {
    let root = repo_root.join("docs/acceptance-cases");
    let mut files = Vec::new();
    collect_markdown_files(&root, &mut files)?;
    let mut invalid = Vec::new();
    for path in files {
        if path.file_name().and_then(|name| name.to_str()) == Some("README.md") {
            continue;
        }
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let valid_status = ["`passing`", "`partial`", "`regression`", "`deprecated`"]
            .iter()
            .any(|status| content.contains(status));
        if !content.contains("## 当前状态") || !valid_status {
            invalid.push(path.display().to_string());
        }
    }
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "acceptance documents missing a valid current status: {invalid:?}"
        ))
    }
}

fn collect_markdown_files(root: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in
        fs::read_dir(root).map_err(|error| format!("cannot read {}: {error}", root.display()))?
    {
        let path = entry
            .map_err(|error| format!("cannot read directory entry: {error}"))?
            .path();
        if path.is_dir() {
            collect_markdown_files(&path, output)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
            output.push(path);
        }
    }
    Ok(())
}

pub(crate) fn check_lockfile_versions(repo_root: &Path, expected: &str) -> Result<(), String> {
    let lock = fs::read_to_string(repo_root.join("Cargo.lock"))
        .map_err(|error| format!("cannot read Cargo.lock: {error}"))?;
    let mut package_name: Option<String> = None;
    let mut checked = 0_u32;
    let mut mismatches = Vec::new();
    for line in lock.lines() {
        if let Some(name) = line
            .strip_prefix("name = \"")
            .and_then(|value| value.strip_suffix('"'))
        {
            package_name = Some(name.to_string());
            continue;
        }
        let Some(version) = line
            .strip_prefix("version = \"")
            .and_then(|value| value.strip_suffix('"'))
        else {
            continue;
        };
        let Some(name) = package_name.take() else {
            continue;
        };
        if name == "rss-ai-news" || name.starts_with("rss-ai-news-") {
            checked += 1;
            if version != expected {
                mismatches.push(format!("{name}={version}"));
            }
        }
    }
    if checked == 0 {
        return Err("Cargo.lock contains no rss-ai-news workspace packages".to_string());
    }
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "workspace package versions do not match {expected}: {mismatches:?}"
        ))
    }
}

pub(crate) fn check_readme_release_identity(
    repo_root: &Path,
    expected: &str,
) -> Result<(), String> {
    let readme = fs::read_to_string(repo_root.join("README.md"))
        .map_err(|error| format!("cannot read README.md: {error}"))?;
    let version_marker = format!("当前版本：`v{expected}`");
    if !readme.contains(&version_marker) {
        return Err(format!("README is missing {version_marker:?}"));
    }
    if !readme.contains("cargo acceptance run --profile local") {
        return Err("README is missing the Rust acceptance matrix command".to_string());
    }
    Ok(())
}

pub(crate) fn workspace_version(repo_root: &Path) -> Result<String, String> {
    let manifest = fs::read_to_string(repo_root.join("Cargo.toml"))
        .map_err(|error| format!("cannot read Cargo.toml: {error}"))?;
    let workspace_package = manifest
        .split("[workspace.package]")
        .nth(1)
        .and_then(|tail| tail.split("\n[").next())
        .ok_or_else(|| "Cargo.toml has no [workspace.package] section".to_string())?;
    workspace_package
        .lines()
        .map(str::trim)
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
        })
        .map(str::to_string)
        .ok_or_else(|| "workspace.package.version is missing".to_string())
}

pub(crate) fn validate_version(version: &str) -> Result<String, String> {
    let valid = !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
        && version.split('.').count() == 3;
    valid
        .then(|| version.to_string())
        .ok_or_else(|| format!("expected version must be X.Y.Z, got {version:?}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{assert_published_after, check_lockfile_versions, validate_version};

    #[test]
    fn published_after_contract_distinguishes_default_and_explicit() {
        let none = r#"{"status":"success","summary":{"published_after":null}}"#;
        assert!(assert_published_after(none, None).is_ok());
        assert!(assert_published_after(none, Some("2000-01-01T00:00:00Z")).is_err());
        let some = r#"{"status":"success","summary":{"published_after":"2000-01-01T00:00:00Z"}}"#;
        assert!(assert_published_after(some, Some("2000-01-01T00:00:00Z")).is_ok());
    }

    #[test]
    fn version_input_is_strict_semver_triplet() {
        assert_eq!(validate_version("0.7.1").unwrap(), "0.7.1");
        for invalid in ["v0.7.1", "0.7", "0.7.1-rc.1", ""] {
            assert!(validate_version(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn lockfile_check_rejects_workspace_version_drift() {
        let directory = std::env::temp_dir().join(format!(
            "rss-ai-news-acceptance-lock-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("Cargo.lock"),
            "[[package]]\nname = \"rss-ai-news\"\nversion = \"0.7.0\"\n",
        )
        .unwrap();
        assert!(check_lockfile_versions(&directory, "0.7.1").is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
