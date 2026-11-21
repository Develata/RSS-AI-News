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

/// The workflow directory scanned for toolchain installs (every `*.yml` /
/// `*.yaml` file, so a new workflow cannot escape the check) and the
/// Dockerfile whose builder image pins the toolchain.
pub(crate) const WORKFLOW_DIR: &str = ".github/workflows";
pub(crate) const TOOLCHAIN_DOCKERFILE: &str = "docker/Dockerfile";

/// The only expression a `toolchain:` input may use instead of the literal
/// channel; the file must then define `RUST_TOOLCHAIN` as that literal.
pub(crate) const TOOLCHAIN_ENV_REFERENCE: &str = "${{ env.RUST_TOOLCHAIN }}";

/// Every toolchain the workflows install and the Dockerfile builds with must
/// be the `channel` of `rust-toolchain.toml`, the single source of truth
/// (R3-9). In each workflow: every `RUST_TOOLCHAIN:` is that literal, every
/// `toolchain:` input is the literal or exactly [`TOOLCHAIN_ENV_REFERENCE`]
/// (backed by such a definition), and there is one `toolchain:` input per
/// `dtolnay/rust-toolchain` step. Any other expression fails: it could
/// resolve to another compiler. The Dockerfile has exactly one `FROM rust:`.
pub(crate) fn check_toolchain_pins(repo_root: &Path) -> Result<(), String> {
    let toolchain = fs::read_to_string(repo_root.join("rust-toolchain.toml"))
        .map_err(|error| format!("cannot read rust-toolchain.toml: {error}"))?;
    let channel = toolchain_channel(&toolchain)
        .ok_or_else(|| "rust-toolchain.toml has no `channel = \"...\"`".to_string())?;
    let mut problems = Vec::new();

    let mut workflows = fs::read_dir(repo_root.join(WORKFLOW_DIR))
        .map_err(|error| format!("cannot read {WORKFLOW_DIR}: {error}"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read {WORKFLOW_DIR}: {error}"))?;
    workflows.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "yml" || extension == "yaml")
    });
    workflows.sort();
    if workflows.is_empty() {
        problems.push(format!("{WORKFLOW_DIR}: no workflow files found"));
    }
    for path in &workflows {
        let name = format!(
            "{WORKFLOW_DIR}/{}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let content =
            fs::read_to_string(path).map_err(|error| format!("cannot read {name}: {error}"))?;
        problems.extend(workflow_toolchain_problems(&name, &content, &channel));
    }

    let dockerfile = fs::read_to_string(repo_root.join(TOOLCHAIN_DOCKERFILE))
        .map_err(|error| format!("cannot read {TOOLCHAIN_DOCKERFILE}: {error}"))?;
    let images = toolchain_lines(&dockerfile)
        .into_iter()
        .filter(|line| line.kind == PinKind::RustImage)
        .collect::<Vec<_>>();
    if images.len() != 1 {
        problems.push(format!(
            "{TOOLCHAIN_DOCKERFILE}: found {} `FROM rust:` image(s), expected 1",
            images.len()
        ));
    }
    for image in images {
        if image.value != channel {
            problems.push(format!(
                "{TOOLCHAIN_DOCKERFILE}:{}: pins {}, expected {channel}",
                image.line, image.value
            ));
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

/// The problems of one workflow file (see [`check_toolchain_pins`]).
fn workflow_toolchain_problems(file: &str, content: &str, channel: &str) -> Vec<String> {
    let lines = toolchain_lines(content);
    let mut problems = Vec::new();
    let mut env_defined = false;
    let mut inputs = 0_usize;
    let mut installs = 0_usize;
    for pin in &lines {
        match pin.kind {
            PinKind::EnvDefinition => {
                env_defined = true;
                if pin.value != channel {
                    problems.push(format!(
                        "{file}:{}: RUST_TOOLCHAIN is {}, expected the literal {channel}",
                        pin.line, pin.value
                    ));
                }
            }
            PinKind::Input => {
                inputs += 1;
                if pin.value != channel && pin.value != TOOLCHAIN_ENV_REFERENCE {
                    problems.push(format!(
                        "{file}:{}: toolchain input is {}, expected {channel} or {TOOLCHAIN_ENV_REFERENCE}",
                        pin.line, pin.value
                    ));
                }
            }
            PinKind::Install => installs += 1,
            PinKind::RustImage => {}
        }
    }
    let uses_reference = lines
        .iter()
        .any(|pin| pin.kind == PinKind::Input && pin.value == TOOLCHAIN_ENV_REFERENCE);
    if uses_reference && !env_defined {
        problems.push(format!(
            "{file}: uses {TOOLCHAIN_ENV_REFERENCE} without defining RUST_TOOLCHAIN"
        ));
    }
    if inputs != installs {
        problems.push(format!(
            "{file}: {installs} dtolnay/rust-toolchain step(s) but {inputs} toolchain input(s)"
        ));
    }
    problems
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PinKind {
    /// `RUST_TOOLCHAIN: <value>`.
    EnvDefinition,
    /// `toolchain: <value>` (an action input).
    Input,
    /// `uses: dtolnay/rust-toolchain@…` (value: the ref).
    Install,
    /// The version of a Dockerfile `FROM rust:X-…` image, with or without an
    /// `@sha256:` digest.
    RustImage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PinLine {
    line: usize,
    kind: PinKind,
    value: String,
}

/// Toolchain-relevant lines with their 1-based line numbers; values are
/// unquoted and stripped of a trailing ` # comment`. Comment lines are skipped.
fn toolchain_lines(content: &str) -> Vec<PinLine> {
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (kind, value) = if let Some(image) = line.strip_prefix("FROM rust:") {
                let tag = image.split_whitespace().next()?;
                let tag = tag.split('@').next()?;
                (PinKind::RustImage, tag.split('-').next()?.to_string())
            } else if let Some(value) = line.strip_prefix("RUST_TOOLCHAIN:") {
                (PinKind::EnvDefinition, value_of(value))
            } else if let Some(value) = line.strip_prefix("toolchain:") {
                (PinKind::Input, value_of(value))
            } else {
                let action = line
                    .strip_prefix("- uses:")
                    .or_else(|| line.strip_prefix("uses:"))?
                    .trim();
                let reference = action.strip_prefix("dtolnay/rust-toolchain@")?;
                (PinKind::Install, value_of(reference))
            };
            Some(PinLine {
                line: index + 1,
                kind,
                value,
            })
        })
        .collect()
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

fn value_of(raw: &str) -> String {
    unquote(raw.split(" #").next().unwrap_or_default()).to_string()
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
/// The compose file's effective scheduler image (the
/// `services.rss-ai-news-scheduler.image` value, not a comment) must also be
/// exactly [`compose_scheduler_image`] (R3-10).
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
        let compose = *file == COMPOSE_FILE;
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
        if compose {
            let wanted = compose_scheduler_image(expected);
            match compose_service_images(&content, COMPOSE_SCHEDULER_SERVICE).as_slice() {
                [(_, image)] if *image == wanted => {}
                [(line, image)] => problems.push(format!(
                    "{file}:{line}: services.{COMPOSE_SCHEDULER_SERVICE}.image is {image}, expected {wanted}"
                )),
                images => problems.push(format!(
                    "{file}: expected one services.{COMPOSE_SCHEDULER_SERVICE}.image, found {}",
                    images.len()
                )),
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

pub(crate) const COMPOSE_FILE: &str = "docker/docker-compose.scheduler.yml";
pub(crate) const COMPOSE_SCHEDULER_SERVICE: &str = "rss-ai-news-scheduler";

/// The one accepted value of the compose scheduler service's `image:`: the
/// version variable with the candidate as its complete default, and the
/// `-scheduler` suffix.
pub(crate) fn compose_scheduler_image(expected: &str) -> String {
    format!("{IMAGE_PREFIX}{COMPOSE_VERSION_VARIABLE}{expected}}}-scheduler")
}

/// `(line, value)` of every `image:` key directly under `services.<service>`
/// in a compose file, by indentation (comment and blank lines ignored, a
/// trailing ` # comment` and quotes stripped). Nested keys of the service
/// (e.g. under `environment:`) are not its image.
fn compose_service_images(content: &str, service: &str) -> Vec<(usize, String)> {
    let mut images = Vec::new();
    let mut in_services = false;
    // Indent of the service's key line, and of its direct children.
    let mut service_indent: Option<usize> = None;
    let mut child_indent: Option<usize> = None;
    for (index, raw) in content.lines().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = raw.len() - trimmed.len();
        let key_line = trimmed.trim_end();
        if indent == 0 {
            in_services = key_line == "services:";
            service_indent = None;
            continue;
        }
        if !in_services {
            continue;
        }
        if let Some(own) = service_indent
            && indent <= own
        {
            service_indent = None;
        }
        if service_indent.is_none() {
            if key_line == format!("{service}:") {
                service_indent = Some(indent);
                child_indent = None;
            }
            continue;
        }
        let children = *child_indent.get_or_insert(indent);
        if indent == children
            && let Some(value) = trimmed.strip_prefix("image:")
        {
            images.push((index + 1, value_of(value)));
        }
    }
    images
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
/// not version pins. The whole tag is the version: `0.10.4-rc.1` is reported
/// as such, not read as its `0.10.4` prefix (R3-10); only a `-scheduler`
/// suffix and a sentence-ending `.` are stripped.
fn image_version_references(content: &str) -> Vec<ImageVersionReference> {
    let is_tag_char = |ch: char| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_';
    let mut references = Vec::new();
    for (index, line) in content.lines().enumerate() {
        for (offset, _) in line.match_indices(IMAGE_PREFIX) {
            let tag = &line[offset + IMAGE_PREFIX.len()..];
            let (version, from_variable) = match tag.strip_prefix(COMPOSE_VERSION_VARIABLE) {
                Some(rest) => {
                    let Some((default, after)) = rest.split_once('}') else {
                        continue;
                    };
                    let suffix_end = after.find(|ch| !is_tag_char(ch)).unwrap_or(after.len());
                    (format!("{default}{}", &after[..suffix_end]), true)
                }
                None => {
                    let end = tag.find(|ch| !is_tag_char(ch)).unwrap_or(tag.len());
                    (tag[..end].to_string(), false)
                }
            };
            let version = version.trim_end_matches('.');
            let version = version.strip_suffix("-scheduler").unwrap_or(version);
            if version.starts_with(|ch: char| ch.is_ascii_digit()) {
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

    use super::{PinKind, check_toolchain_pins, toolchain_lines};

    const INSTALL: &str = "      - uses: dtolnay/rust-toolchain@abc # master\n        with:\n";

    /// A scratch repository with the given workflow files and Dockerfile.
    fn toolchain_repo(name: &str, channel: &str, files: &[(&str, String)]) -> std::path::PathBuf {
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
        for (file, content) in files {
            fs::write(root.join(file), content).unwrap();
        }
        root
    }

    fn consistent_files(version: &str) -> Vec<(&'static str, String)> {
        vec![
            (
                ".github/workflows/ci.yml",
                format!(
                    "env:\n  RUST_TOOLCHAIN: \"{version}\" # keep equal\njobs:\n  test:\n    steps:\n\
                     {INSTALL}          toolchain: ${{{{ env.RUST_TOOLCHAIN }}}}\n"
                ),
            ),
            (
                ".github/workflows/audit.yml",
                format!("{INSTALL}          toolchain: '{version}'\n"),
            ),
            (
                ".github/workflows/maintenance.yml",
                format!(
                    "{INSTALL}          toolchain: '{version}'\n{INSTALL}          toolchain: '{version}'\n"
                ),
            ),
            (
                "docker/Dockerfile",
                format!("FROM rust:{version}-slim-bookworm@sha256:0123abcd AS chef\n"),
            ),
        ]
    }

    fn check_with(name: &str, edit: impl FnOnce(&mut Vec<(&'static str, String)>)) -> String {
        let mut files = consistent_files("1.97.0");
        edit(&mut files);
        let root = toolchain_repo(name, "1.97.0", &files);
        let result = check_toolchain_pins(&root);
        fs::remove_dir_all(root).unwrap();
        result.err().unwrap_or_default()
    }

    #[test]
    fn toolchain_lines_recognise_every_pin_form() {
        let lines = toolchain_lines(
            "  RUST_TOOLCHAIN: \"1.97.0\"\n  toolchain: '1.97.0'\n  toolchain: ${{ env.X }}\n\
             # toolchain: '9.9.9'\nFROM rust:1.97.0-slim-bookworm@sha256:ab AS chef\n\
             FROM debian:bookworm-slim AS runtime\n      - uses: dtolnay/rust-toolchain@abc # m\n",
        );
        let kinds = lines
            .iter()
            .map(|pin| (pin.line, pin.kind, pin.value.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                (1, PinKind::EnvDefinition, "1.97.0"),
                (2, PinKind::Input, "1.97.0"),
                (3, PinKind::Input, "${{ env.X }}"),
                (5, PinKind::RustImage, "1.97.0"),
                (7, PinKind::Install, "abc"),
            ]
        );
    }

    #[test]
    fn toolchain_check_accepts_consistent_pins() {
        assert_eq!(check_with("ok", |_| {}), "");
    }

    #[test]
    fn toolchain_check_rejects_a_drifted_pin() {
        let error = check_with("drift", |files| {
            files[3].1 = "FROM rust:1.96.0-slim-bookworm AS chef\n".to_string();
        });
        assert!(
            error.contains("docker/Dockerfile:1: pins 1.96.0, expected 1.97.0"),
            "{error}"
        );
    }

    /// R3-9: an expression other than the checked env reference could
    /// resolve to another compiler; it used to be skipped as "not a pin".
    #[test]
    fn toolchain_check_rejects_an_unchecked_expression() {
        let error = check_with("expr", |files| {
            files[1].1 = format!("{INSTALL}          toolchain: ${{{{ '1.96.0' }}}}\n");
        });
        assert!(
            error.contains(".github/workflows/audit.yml:3: toolchain input is ${{ '1.96.0' }}"),
            "{error}"
        );
        let error = check_with("env-expr", |files| {
            files[0].1 = files[0].1.replace("\"1.97.0\"", "${{ vars.RUST }}");
        });
        assert!(
            error.contains("RUST_TOOLCHAIN is ${{ vars.RUST }}"),
            "{error}"
        );
    }

    #[test]
    fn toolchain_check_rejects_a_reference_without_a_definition() {
        let error = check_with("undefined", |files| {
            files[1].1 = format!("{INSTALL}          toolchain: ${{{{ env.RUST_TOOLCHAIN }}}}\n");
        });
        assert!(
            error.contains(
                ".github/workflows/audit.yml: uses ${{ env.RUST_TOOLCHAIN }} without defining"
            ),
            "{error}"
        );
    }

    /// An install step without a toolchain input (format change, deleted
    /// line) must fail rather than leave the check with nothing to compare.
    #[test]
    fn toolchain_check_rejects_an_install_without_input() {
        let error = check_with("missing", |files| {
            files[2].1 = format!("{INSTALL}          toolchain: '1.97.0'\n{INSTALL}");
        });
        assert!(
            error.contains(
                ".github/workflows/maintenance.yml: 2 dtolnay/rust-toolchain step(s) but 1 toolchain input(s)"
            ),
            "{error}"
        );
    }

    /// Every workflow file is scanned, not a fixed list.
    #[test]
    fn toolchain_check_scans_new_workflow_files() {
        let error = check_with("new-file", |files| {
            files.push((
                ".github/workflows/nightly.yaml",
                format!("{INSTALL}          toolchain: '1.96.0'\n"),
            ));
        });
        assert!(
            error.contains(".github/workflows/nightly.yaml:3"),
            "{error}"
        );
    }

    /// The repository itself must pass, so the lane fails on real drift only.
    #[test]
    fn repository_toolchain_pins_are_consistent() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(check_toolchain_pins(&root), Ok(()));
    }

    use super::{
        COMPOSE_SCHEDULER_SERVICE, DEPLOYMENT_VERSION_FILES, check_deployment_image_versions,
        compose_scheduler_image, compose_service_images, image_version_references,
        workspace_version,
    };

    /// A compose file whose scheduler service uses `image`, with the
    /// candidate version also named in a comment above it.
    fn compose_with(image: &str) -> String {
        format!(
            "# docker run ghcr.io/develata/rss-ai-news:${{RSS_AI_NEWS_VERSION:-0.10.4}} migrate run\n\
             services:\n  {COMPOSE_SCHEDULER_SERVICE}:\n    # image: the scheduler image\n\
             \x20   image: {image}\n    environment:\n      - image: not-the-service-image\n"
        )
    }

    #[test]
    fn image_version_references_parse_literal_and_variable_tags() {
        let references = image_version_references(
            "image: ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4}-scheduler\n\
             | `ghcr.io/develata/rss-ai-news:<tag>` | `ghcr.io/develata/rss-ai-news:latest` |\n\
             docker run ghcr.io/develata/rss-ai-news:0.7.0 \\\n\
             see ghcr.io/develata/rss-ai-news:0.10.4.\n\
             ghcr.io/develata/rss-ai-news:0.10.4-rc.1 and ghcr.io/develata/rss-ai-news:0.10.4-scheduler\n\
             ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4-rc.1}-scheduler\n",
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
                (4, "0.10.4", false),
                (5, "0.10.4-rc.1", false),
                (5, "0.10.4", false),
                (6, "0.10.4-rc.1", true)
            ]
        );
    }

    #[test]
    fn compose_service_image_ignores_comments_and_nested_keys() {
        let compose = compose_with("ghcr.io/develata/rss-ai-news:latest-scheduler # pinned");
        assert_eq!(
            compose_service_images(&compose, COMPOSE_SCHEDULER_SERVICE),
            vec![(
                5,
                "ghcr.io/develata/rss-ai-news:latest-scheduler".to_string()
            )]
        );
        assert!(compose_service_images("image: x\n", COMPOSE_SCHEDULER_SERVICE).is_empty());
    }

    /// R3-10: the comment names the candidate, but the service runs another
    /// image; formerly the whole-text scan passed it.
    #[test]
    fn deployment_versions_check_the_effective_compose_image() {
        for (name, image, expected_error) in [
            (
                "latest",
                "ghcr.io/develata/rss-ai-news:latest-scheduler",
                "services.rss-ai-news-scheduler.image is ghcr.io/develata/rss-ai-news:latest-scheduler",
            ),
            (
                "runtime",
                "ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4}",
                "services.rss-ai-news-scheduler.image is ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4},",
            ),
            (
                "rc",
                "ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4-rc.1}-scheduler",
                "names 0.10.4-rc.1, expected 0.10.4",
            ),
            (
                "other-variable",
                "ghcr.io/develata/rss-ai-news:${OTHER:-0.10.4}-scheduler",
                "services.rss-ai-news-scheduler.image is",
            ),
        ] {
            let root = deployment_repo(
                name,
                &compose_with(image),
                "ghcr.io/develata/rss-ai-news:0.10.4\n",
            );
            let error = check_deployment_image_versions(&root, "0.10.4").unwrap_err();
            assert!(error.contains(expected_error), "{name}: {error}");
            fs::remove_dir_all(root).unwrap();
        }
        assert_eq!(
            compose_scheduler_image("0.10.4"),
            "ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4}-scheduler"
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
            &compose_with("ghcr.io/develata/rss-ai-news:${RSS_AI_NEWS_VERSION:-0.10.4}-scheduler"),
            "  ghcr.io/develata/rss-ai-news:0.10.4 \\\n",
        );
        assert_eq!(check_deployment_image_versions(&root, "0.10.4"), Ok(()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deployment_versions_reject_stale_hard_coded_and_missing_versions() {
        let root = deployment_repo(
            "stale",
            &compose_with("ghcr.io/develata/rss-ai-news:0.7.0-scheduler"),
            "no image here\n",
        );
        let error = check_deployment_image_versions(&root, "0.10.4").unwrap_err();
        for expected in [
            "docker/docker-compose.scheduler.yml:5: hard-codes 0.7.0",
            "docker/docker-compose.scheduler.yml:5: names 0.7.0, expected 0.10.4",
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
