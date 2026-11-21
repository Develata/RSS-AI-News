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

/// Deployment files that name a published image version. The compose file
/// must take it from `${RSS_AI_NEWS_VERSION:-X.Y.Z}`; the documents' example
/// commands may name it literally. Placeholders such as `<tag>` are skipped.
pub(crate) const DEPLOYMENT_VERSION_FILES: &[&str] = &[
    "docker/docker-compose.scheduler.yml",
    "docs/operations/docker-and-scheduler.md",
    "docs/operations/postgres-deployment.md",
    "docs/plan/12-deployment.md",
    "README.md",
];
const IMAGE_PREFIX: &str = "ghcr.io/develata/rss-ai-news:";
const COMPOSE_VERSION_VARIABLE: &str = "${RSS_AI_NEWS_VERSION:-";

/// Every image version named in `DEPLOYMENT_VERSION_FILES` must be the
/// release candidate, each file must name at least one (so a format change
/// cannot pass vacuously), and the compose file must not hard-code one.
pub(crate) fn check_deployment_image_versions(
    repo_root: &Path,
    expected: &str,
) -> Result<(), String> {
    let mut problems = Vec::new();
    for file in DEPLOYMENT_VERSION_FILES {
        let content = fs::read_to_string(repo_root.join(file))
            .map_err(|error| format!("cannot read {file}: {error}"))?;
        let references = image_version_references(&content);
        if references.is_empty() {
            problems.push(format!("{file}: names no {IMAGE_PREFIX}<version> image"));
        }
        let compose = file.ends_with(".yml");
        for reference in references {
            if compose && !reference.from_variable {
                problems.push(format!(
                    "{file}:{}: hard-codes {}; use {COMPOSE_VERSION_VARIABLE}{expected}}}",
                    reference.line, reference.version
                ));
            }
            if reference.version != expected {
                problems.push(format!(
                    "{file}:{}: names {}, expected {expected}",
                    reference.line, reference.version
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "deployment image versions do not match {expected}: {problems:?}"
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ImageVersionReference {
    line: usize,
    version: String,
    from_variable: bool,
}

/// `ghcr.io/develata/rss-ai-news:X.Y.Z[-scheduler]` and
/// `ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-X.Y.Z}[-scheduler]`
/// references with 1-based line numbers. Other tags (`<tag>`, `latest`) are
/// not version pins.
fn image_version_references(content: &str) -> Vec<ImageVersionReference> {
    let mut references = Vec::new();
    for (index, line) in content.lines().enumerate() {
        for (offset, _) in line.match_indices(IMAGE_PREFIX) {
            let tag = &line[offset + IMAGE_PREFIX.len()..];
            let (tag, from_variable) = match tag.strip_prefix(COMPOSE_VERSION_VARIABLE) {
                Some(rest) => (rest, true),
                None => (tag, false),
            };
            let version = tag
                .split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
                .next()
                .unwrap_or_default()
                .trim_end_matches('.');
            if version.split('.').count() == 3 && version.split('.').all(|part| !part.is_empty()) {
                references.push(ImageVersionReference {
                    line: index + 1,
                    version: version.to_string(),
                    from_variable,
                });
            }
        }
    }
    references
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

/// `X.Y.Z` with each component `0` or a digit run without a leading zero
/// (SemVer 2.0.0 §2), the same rule as `.github/scripts/release-promotion.sh`.
pub(crate) fn validate_version(version: &str) -> Result<String, String> {
    let valid = version.split('.').count() == 3
        && version.split('.').all(|part| {
            !part.is_empty()
                && part.chars().all(|ch| ch.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'))
        });
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
        for invalid in [
            "v0.7.1",
            "0.7",
            "0.7.1-rc.1",
            "",
            "01.7.1",
            "0.07.1",
            "0.7.01",
            "0.7.1.",
        ] {
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
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
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

    use super::{
        DEPLOYMENT_VERSION_FILES, check_deployment_image_versions, image_version_references,
        workspace_version,
    };

    #[test]
    fn image_version_references_parse_literal_and_variable_tags() {
        let references = image_version_references(
            "image: ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4}-scheduler\n\
             | `ghcr.io/develata/rss-ai-news:<tag>` | `ghcr.io/develata/rss-ai-news:latest` |\n\
             docker run ghcr.io/develata/rss-ai-news:0.7.0 \\\n\
             see ghcr.io/develata/rss-ai-news:0.10.4.\n",
        );
        let found = references
            .iter()
            .map(|reference| {
                (
                    reference.line,
                    reference.version.as_str(),
                    reference.from_variable,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            vec![
                (1, "0.10.4", true),
                (3, "0.7.0", false),
                (4, "0.10.4", false)
            ]
        );
    }

    fn deployment_repo(name: &str, compose: &str, document: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rss-ai-news-acceptance-deploy-{name}-{}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        for file in DEPLOYMENT_VERSION_FILES {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let content = if file.ends_with(".yml") {
                compose
            } else {
                document
            };
            fs::write(path, content).unwrap();
        }
        root
    }

    #[test]
    fn deployment_versions_accept_the_candidate() {
        let root = deployment_repo(
            "ok",
            "image: ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4}-scheduler\n",
            "  ghcr.io/develata/rss-ai-news:0.10.4 \\\n",
        );
        assert_eq!(check_deployment_image_versions(&root, "0.10.4"), Ok(()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deployment_versions_reject_stale_hard_coded_and_missing_versions() {
        let root = deployment_repo(
            "stale",
            "image: ghcr.io/develata/rss-ai-news:0.7.0-scheduler\n",
            "no image here\n",
        );
        let error = check_deployment_image_versions(&root, "0.10.4").unwrap_err();
        for expected in [
            "docker/docker-compose.scheduler.yml:1: hard-codes 0.7.0",
            "docker/docker-compose.scheduler.yml:1: names 0.7.0, expected 0.10.4",
            "docs/plan/12-deployment.md: names no ghcr.io/develata/rss-ai-news:<version> image",
        ] {
            assert!(error.contains(expected), "{expected} not in {error}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    /// The repository's deployment files name the workspace version, so the
    /// release lane fails only when a bump misses one of them.
    #[test]
    fn repository_deployment_versions_match_the_workspace() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let version = workspace_version(&root).unwrap();
        assert_eq!(check_deployment_image_versions(&root, &version), Ok(()));
    }
}
