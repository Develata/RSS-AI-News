//! F7-2 W3-4 medium —— 防御纵深守护：禁止 secret 通过 tracing / println
//! / eprintln 流入日志/标准输出。
//!
//! 背景：`SecretString` 在类型层已经把 `Debug` / `Display` 实现 redact 为
//! `"***"`（详见 `redact.rs` + `AiClientConfig` / `GitHubTargetConfig` 的
//! 自定义 Debug 回归测试），但只要代码主动 `.expose_secret()` 拿到原始
//! 字符串再喂给 tracing 宏或 println，就会绕过类型层防御。
//!
//! 该测试是 PR-time 守护：扫描所有生产代码（`crates/*/src/**/*.rs`，
//! 跳过 `tests/` 与 `benches/`），任何同时出现 `tracing::` / `println!`
//! / `eprintln!` 与 `.expose_secret()` 的真实代码行都视为违反。
//!
//! 该 grep 只能拦截一种最常见的 pattern（同行内同时出现宏调用与
//! `expose_secret`）。"先 `let k = x.expose_secret()` 再 trace `{k}`"
//! 这类跨行场景需要靠 docs/design/error-and-observability.md §4.2
//! "日志/artifact 不变量" 的代码规范 + code review 把关。
//!
//! 详见 W3 audit findings W3-4 与 F7-2 commit message。

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // env!("CARGO_MANIFEST_DIR") = .../crates/observability
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p
}

fn walk_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    if !root.is_dir() {
        return;
    }
    let entries = match fs::read_dir(root) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if path.is_dir() {
            // 跳过 cargo / IDE 副产物
            if matches!(name, "target" | ".git" | "node_modules") {
                continue;
            }
            walk_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn is_production_source(path: &Path) -> bool {
    let mut in_src = false;
    let mut in_skip = false;
    for c in path.components() {
        match c.as_os_str().to_str() {
            Some("src") => in_src = true,
            Some("tests") | Some("benches") | Some("examples") => in_skip = true,
            _ => {}
        }
    }
    in_src && !in_skip
}

#[test]
fn no_secret_exposed_to_tracing_or_println_in_production_code() {
    let crates_dir = workspace_root().join("crates");
    let mut files = Vec::new();
    walk_rs_files(&crates_dir, &mut files);

    assert!(
        !files.is_empty(),
        "walker found no .rs files under {}; test setup is broken",
        crates_dir.display(),
    );

    let mut violations: Vec<String> = Vec::new();
    for path in files.iter().filter(|p| is_production_source(p)) {
        let body = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (idx, line) in body.lines().enumerate() {
            let trimmed = line.trim_start();
            // 跳过 // 行注释与 /// 文档注释、块注释续行 *、宏属性、
            // 以及含有 `expose_secret` 字面量但本身在引号内的纯文档
            // 描述。组合判定（同行 tracing/println + expose_secret）
            // 在生产代码中已经极少误报；进一步的字符串字面量识别
            // 会让该测试变得脆弱，无必要。
            if trimmed.starts_with("//") || trimmed.starts_with('*') {
                continue;
            }
            let has_log_macro = line.contains("tracing::")
                || line.contains("println!")
                || line.contains("eprintln!");
            let has_expose = line.contains("expose_secret");
            if has_log_macro && has_expose {
                violations.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    idx + 1,
                    line.trim_end()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "W3-4 invariant violation: secret exposure via tracing/println/eprintln \
         macros detected in production source.\n\
         必须改用 SecretString 类型穿透或在 SDK boundary 单独构造原值。\n\
         详见 docs/design/error-and-observability.md §4.2 \"日志/artifact 不变量\"。\n\
         Offending lines:\n{}",
        violations.join("\n"),
    );
}
