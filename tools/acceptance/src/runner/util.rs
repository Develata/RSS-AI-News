use std::{
    env,
    path::{Path, PathBuf},
};

const OUTPUT_TAIL_BYTES: usize = 16 * 1024;

pub(crate) fn resolve_target_dir(repo_root: &Path, explicit: Option<&Path>) -> PathBuf {
    let configured = explicit
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| repo_root.join("target"));
    if configured.is_absolute() {
        configured
    } else {
        repo_root.join(configured)
    }
}

pub(crate) fn release_binary(target_dir: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "rss-ai-news.exe"
    } else {
        "rss-ai-news"
    };
    target_dir.join("release").join(name)
}

pub(crate) fn display_command(program: &Path, args: &[String], envs: &[(&str, &str)]) -> String {
    let mut parts = envs
        .iter()
        .map(|(key, _)| format!("{key}=<redacted>"))
        .collect::<Vec<_>>();
    parts.push(shell_word(&program.display().to_string()));
    parts.extend(args.iter().map(|arg| shell_word(arg)));
    parts.join(" ")
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-._/:={}*".contains(ch))
    {
        value.to_string()
    } else {
        format!("{:?}", value)
    }
}

pub(crate) fn strings<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.into_iter().map(str::to_string).collect()
}

pub(crate) fn tail(value: &str) -> String {
    if value.len() <= OUTPUT_TAIL_BYTES {
        return value.to_string();
    }
    let mut start = value.len() - OUTPUT_TAIL_BYTES;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!("[truncated]\n{}", &value[start..])
}

pub(crate) fn program_exists(program: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|directory| {
        let candidate = directory.join(program);
        candidate.is_file() || (cfg!(windows) && directory.join(format!("{program}.exe")).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::tail;

    #[test]
    fn output_tail_is_bounded_and_marks_truncation() {
        let input = "x".repeat(20_000);
        let output = tail(&input);
        assert!(output.starts_with("[truncated]\n"));
        assert!(output.len() < input.len());
    }
}
