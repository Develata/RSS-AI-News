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

/// CLI output contract (docs/plan/11 §5): stdout is exactly one JSON document
/// and its `status` agrees with the process exit code.
pub(crate) fn assert_single_json_document(
    stdout: &str,
    truncated: bool,
    exit_code: i32,
) -> Result<(), String> {
    if truncated {
        return Err("stdout exceeded the capture limit".to_string());
    }
    let documents = serde_json::Deserializer::from_str(stdout)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("stdout is not a JSON document stream: {error}"))?;
    let [document] = documents.as_slice() else {
        return Err(format!(
            "expected exactly one JSON document on stdout, found {}",
            documents.len()
        ));
    };
    let status = document.get("status").and_then(Value::as_str);
    let consistent = match exit_code {
        0 => matches!(status, Some("success" | "ok" | "warn")),
        _ => matches!(status, Some("fail" | "error")),
    };
    if consistent {
        Ok(())
    } else {
        Err(format!(
            "status {status:?} contradicts exit code {exit_code}"
        ))
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

/// Files that pin the Rust toolchain outside `rust-toolchain.toml`, with the
/// number of literal pins each must contain. The counts make a format change
/// (a pin this parser no longer recognises) fail instead of passing vacuously.
pub(crate) const TOOLCHAIN_PIN_FILES: &[(&str, usize)] = &[
    (".github/workflows/ci.yml", 1),
    (".github/workflows/release.yml", 1),
    (".github/workflows/audit.yml", 1),
    (".github/workflows/maintenance.yml", 2),
    ("docker/Dockerfile", 1),
];

/// Every literal toolchain pin in `TOOLCHAIN_PIN_FILES` must equal the
/// `channel` of `rust-toolchain.toml`, the single source of truth.
pub(crate) fn check_toolchain_pins(repo_root: &Path) -> Result<(), String> {
    let toolchain = fs::read_to_string(repo_root.join("rust-toolchain.toml"))
        .map_err(|error| format!("cannot read rust-toolchain.toml: {error}"))?;
    let channel = toolchain_channel(&toolchain)
        .ok_or_else(|| "rust-toolchain.toml has no `channel = \"...\"`".to_string())?;
    let mut problems = Vec::new();
    for (file, expected_count) in TOOLCHAIN_PIN_FILES {
        let content = fs::read_to_string(repo_root.join(file))
            .map_err(|error| format!("cannot read {file}: {error}"))?;
        let pins = toolchain_pins(&content);
        if pins.len() != *expected_count {
            problems.push(format!(
                "{file}: found {} toolchain pin(s), expected {expected_count} \
                 (update TOOLCHAIN_PIN_FILES if the layout changed on purpose)",
                pins.len()
            ));
        }
        for (line, value) in pins {
            if value != channel {
                problems.push(format!("{file}:{line}: pins {value}, expected {channel}"));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "toolchain pins drift from rust-toolchain.toml: {problems:?}"
        ))
    }
}

fn toolchain_channel(toolchain: &str) -> Option<String> {
    toolchain.lines().map(str::trim).find_map(|line| {
        let value = line
            .strip_prefix("channel")?
            .trim_start()
            .strip_prefix('=')?;
        Some(unquote(value).to_string())
    })
}

/// Literal toolchain pins with their 1-based line numbers: workflow
/// `RUST_TOOLCHAIN: "X"` / `toolchain: 'X'` values (expressions such as
/// `${{ env.RUST_TOOLCHAIN }}` are references, not pins) and the version of a
/// Dockerfile `FROM rust:X-…` image, with or without an `@sha256:` digest.
fn toolchain_pins(content: &str) -> Vec<(usize, String)> {
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            let value = if let Some(image) = line.strip_prefix("FROM rust:") {
                let tag = image.split_whitespace().next()?;
                let tag = tag.split('@').next()?;
                tag.split('-').next()?.to_string()
            } else {
                let value = line
                    .strip_prefix("RUST_TOOLCHAIN:")
                    .or_else(|| line.strip_prefix("toolchain:"))?;
                let value = unquote(value.split(" #").next()?);
                if value.contains("${{") {
                    return None;
                }
                value.to_string()
            };
            Some((index + 1, value))
        })
        .collect()
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(value)
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
    use super::assert_single_json_document;

    #[test]
    fn single_json_document_contract() {
        assert!(assert_single_json_document(r#"{"status":"success"}"#, false, 0).is_ok());
        assert!(assert_single_json_document("{\"status\":\"fail\"}\n", false, 1).is_ok());
        // Two documents (the old doctor behaviour).
        assert!(
            assert_single_json_document(r#"{"status":"fail"} {"status":"error"}"#, false, 1)
                .is_err()
        );
        // Markdown before the envelope (the old rebuild-report behaviour).
        assert!(assert_single_json_document("# Report\n{\"status\":\"ok\"}", false, 0).is_err());
        // Status contradicts exit code.
        assert!(assert_single_json_document(r#"{"status":"success"}"#, false, 1).is_err());
        assert!(assert_single_json_document(r#"{"status":"success"}"#, true, 0).is_err());
    }

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

    use super::{TOOLCHAIN_PIN_FILES, check_toolchain_pins, toolchain_pins};

    /// A scratch repository holding every toolchain pin file.
    fn toolchain_repo(name: &str, channel: &str, pins: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rss-ai-news-acceptance-toolchain-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::create_dir_all(root.join("docker")).unwrap();
        fs::write(
            root.join("rust-toolchain.toml"),
            format!("[toolchain]\nchannel = \"{channel}\"\n"),
        )
        .unwrap();
        for (file, content) in pins {
            fs::write(root.join(file), content).unwrap();
        }
        root
    }

    fn consistent_pins(version: &str) -> Vec<(&'static str, String)> {
        vec![
            (
                ".github/workflows/ci.yml",
                format!(
                    "env:\n  RUST_TOOLCHAIN: \"{version}\"\njobs:\n  test:\n    steps:\n      \
                     - uses: dtolnay/rust-toolchain@abc # master\n        with:\n          \
                     toolchain: ${{{{ env.RUST_TOOLCHAIN }}}}\n"
                ),
            ),
            (
                ".github/workflows/release.yml",
                format!("env:\n  RUST_TOOLCHAIN: \"{version}\" # keep equal\n"),
            ),
            (
                ".github/workflows/audit.yml",
                format!("        with:\n          toolchain: '{version}'\n"),
            ),
            (
                ".github/workflows/maintenance.yml",
                format!("          toolchain: '{version}'\n          toolchain: '{version}'\n"),
            ),
            (
                "docker/Dockerfile",
                format!("FROM rust:{version}-slim-bookworm@sha256:0123abcd AS chef\n"),
            ),
        ]
    }

    fn write_repo(
        name: &str,
        channel: &str,
        pins: &[(&'static str, String)],
    ) -> std::path::PathBuf {
        let borrowed = pins
            .iter()
            .map(|(file, content)| (*file, content.as_str()))
            .collect::<Vec<_>>();
        toolchain_repo(name, channel, &borrowed)
    }

    #[test]
    fn toolchain_pins_recognise_every_pin_form() {
        let pins = toolchain_pins(
            "  RUST_TOOLCHAIN: \"1.97.0\"\n  toolchain: '1.97.0'\n  toolchain: ${{ env.X }}\n\
             # toolchain: '9.9.9'\nFROM rust:1.97.0-slim-bookworm@sha256:ab AS chef\n\
             FROM debian:bookworm-slim AS runtime\n",
        );
        assert_eq!(
            pins,
            vec![
                (1, "1.97.0".to_string()),
                (2, "1.97.0".to_string()),
                (5, "1.97.0".to_string())
            ]
        );
    }

    #[test]
    fn toolchain_check_accepts_consistent_pins() {
        let root = write_repo("ok", "1.97.0", &consistent_pins("1.97.0"));
        assert_eq!(check_toolchain_pins(&root), Ok(()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn toolchain_check_rejects_a_drifted_pin() {
        let mut pins = consistent_pins("1.97.0");
        pins[4].1 = "FROM rust:1.96.0-slim-bookworm AS chef\n".to_string();
        let root = write_repo("drift", "1.97.0", &pins);
        let error = check_toolchain_pins(&root).unwrap_err();
        assert!(
            error.contains("docker/Dockerfile:1: pins 1.96.0, expected 1.97.0"),
            "{error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A pin the parser no longer finds (format change, deleted line) must
    /// fail rather than leave the check with nothing to compare.
    #[test]
    fn toolchain_check_rejects_a_missing_pin() {
        let mut pins = consistent_pins("1.97.0");
        pins[3].1 = "          toolchain: '1.97.0'\n".to_string();
        let root = write_repo("missing", "1.97.0", &pins);
        let error = check_toolchain_pins(&root).unwrap_err();
        assert!(
            error.contains(
                ".github/workflows/maintenance.yml: found 1 toolchain pin(s), expected 2"
            ),
            "{error}"
        );
        fs::remove_dir_all(root).unwrap();
        assert_eq!(TOOLCHAIN_PIN_FILES.len(), consistent_pins("x").len());
    }

    /// The repository itself must pass, so the lane fails on real drift only.
    #[test]
    fn repository_toolchain_pins_are_consistent() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(check_toolchain_pins(&root), Ok(()));
    }
}
