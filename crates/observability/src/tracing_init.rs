use std::{
    io,
    path::{Path, PathBuf},
};

use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Clone)]
pub struct InitOptions {
    pub log_level: String,
    pub log_format: String,
    /// 日志落盘路径（F15-13 W9-F1）。
    ///   - 空串：仅写 stderr，函数返回 `None`
    ///   - 含目录：`parent` 作目录、`file_name` 作 prefix，按 `prefix.YYYY-MM-DD`
    ///     日轮转（`tracing_appender::rolling::daily`）
    ///   - 纯文件名（无父目录）：当前工作目录作目录、原值作 prefix
    ///   - 无 file_name（如 `.`、`..`、纯根）：用 fallback prefix `"rss-ai-news"`
    ///
    /// `[observability].log_file` 当前由 CLI `--log-file` 直传；config.toml
    /// 的同名字段尚未在 startup init 前被读取（详见 cli/src/lib.rs 注释）。
    pub log_file: String,
}

/// 安装全局 tracing 订阅者。
///
/// 返回值为 `WorkerGuard` 时（仅 `log_file` 非空 + try_init 成功的情况），
/// 调用方**必须**把 guard 持有到进程结束——`tracing-appender` 的 non-blocking
/// writer 通过 channel 把日志投递给后台 worker，guard 在 Drop 时 flush
/// 剩余消息并关闭文件。提前 drop 会导致进程退出前的少量日志被截断。
///
/// 多次调用 `init` 时只有第一次成功安装订阅者；后续调用走 stderr 路径或
/// 直接丢弃刚 spawn 出来的 file appender（channel 关闭 → worker 退出，
/// 无资源泄漏）。
pub fn init(opts: InitOptions) -> Option<WorkerGuard> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&opts.log_level));
    let want_json = opts.log_format == "json";

    let Some((dir, prefix)) = parse_log_file_path(&opts.log_file) else {
        let builder = fmt().with_env_filter(filter).with_writer(io::stderr);
        if want_json {
            let _ = builder.json().try_init();
        } else {
            let _ = builder.try_init();
        }
        return None;
    };

    // rolling::daily 在底层 ensures 目录存在的语义不强保证（它在第一次写
    // 时才 lazy 创建文件）；提前 create_dir_all 让配置错误（path 无写权
    // 或父路径不存在）立即可见。失败 → 降级到 stderr 并打印 warn，避免
    // 用户日志被静默丢弃。
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "[observability] failed to create log directory {dir:?}: {error}; \
             falling back to stderr"
        );
        let builder = fmt().with_env_filter(filter).with_writer(io::stderr);
        if want_json {
            let _ = builder.json().try_init();
        } else {
            let _ = builder.try_init();
        }
        return None;
    }

    let appender = rolling::daily(&dir, &prefix);
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let builder = fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false);
    let init_ok = if want_json {
        builder.json().try_init().is_ok()
    } else {
        builder.try_init().is_ok()
    };

    if init_ok {
        Some(guard)
    } else {
        // 已被前一次 init 安装：丢 guard、channel 关闭、worker 退出。
        // 调用方拿不到 guard 也无须保留——此次 file appender 不会有
        // 任何消息流入。
        drop(guard);
        None
    }
}

/// 把 `InitOptions::log_file` 解析为 `(dir, prefix)` 二元组，给
/// `tracing_appender::rolling::daily` 直用。`None` 表示空串（stderr 模式）。
///
/// 解析规则（见 [`InitOptions::log_file`] 文档）：
///   1. 空串或全空白 → `None`
///   2. 含目录组件 + 文件名 → `(parent, file_name)`
///   3. 纯文件名 → `(".", raw)`
///   4. 无 file_name（如 `.`、`..`、纯根）→ `(raw, "rss-ai-news")`
pub(crate) fn parse_log_file_path(raw: &str) -> Option<(PathBuf, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = Path::new(trimmed);
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            Some((parent.to_path_buf(), name.to_string_lossy().into_owned()))
        }
        (_, Some(name)) => Some((PathBuf::from("."), name.to_string_lossy().into_owned())),
        _ => Some((path.to_path_buf(), "rss-ai-news".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_none() {
        assert!(parse_log_file_path("").is_none());
        assert!(parse_log_file_path("   ").is_none());
    }

    #[test]
    fn pure_filename_falls_back_to_cwd() {
        let (dir, prefix) = parse_log_file_path("app.log").unwrap();
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(prefix, "app.log");
    }

    #[test]
    fn relative_path_splits_parent_and_name() {
        let (dir, prefix) = parse_log_file_path("logs/app.log").unwrap();
        assert_eq!(dir, PathBuf::from("logs"));
        assert_eq!(prefix, "app.log");
    }

    #[test]
    fn nested_path_keeps_full_parent() {
        let (dir, prefix) = parse_log_file_path("var/log/rss/app").unwrap();
        assert_eq!(dir, PathBuf::from("var/log/rss"));
        assert_eq!(prefix, "app");
    }

    #[test]
    fn dot_path_uses_fallback_prefix() {
        // `Path::new(".").file_name()` 返 None（Rust Path 把 `.` 视为
        // 当前目录、无文件名分量）；走 fallback 分支：dir=raw、
        // prefix="rss-ai-news"。`..`、纯根 `/`、`C:\` 等"无 file_name"
        // 路径都进同一支。注意 `"logs/"` 不在此列——Rust Path 会把
        // 末尾分隔符吸收，file_name 仍为 `"logs"`、走纯文件名分支。
        let (dir, prefix) = parse_log_file_path(".").unwrap();
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(prefix, "rss-ai-news");
    }
}
