use std::time::{Duration, Instant};

use rss_ai_news_observability::tracing_init::{InitOptions, init};

/// F15-13 W9-F1：每个 `tests/*.rs` 文件是独立 binary，全局 tracing
/// 订阅者干净——这里专门验证 file-mode 首次 init 的"全链路落盘"契约：
///   1. init 返 `Some(WorkerGuard)`（订阅者真的装上）
///   2. 紧随其后的 `tracing::info!` 经 non_blocking 队列 → rolling
///      worker → 落到 `dir/prefix.YYYY-MM-DD`
///   3. drop guard 后所有缓冲日志已落盘（worker flush）
#[test]
fn file_mode_first_init_writes_log_to_rolling_daily_file() {
    let dir = tempfile::tempdir().expect("temp dir should be created");
    let log_path = dir.path().join("rss-ai-news.log");
    let guard = init(InitOptions {
        log_level: "info".to_string(),
        log_format: "pretty".to_string(),
        log_file: log_path.to_string_lossy().into_owned(),
    });
    assert!(
        guard.is_some(),
        "首次 init + 非空 log_file 应当返回 WorkerGuard"
    );

    tracing::info!("rss-ai-news-test-marker-line");

    // drop guard → tracing-appender worker flush 队列、关闭文件。
    drop(guard);

    // 文件名形如 `rss-ai-news.log.YYYY-MM-DD`。轮询直到匹配文件出现**且**
    // marker 已落盘——non_blocking worker 异步投递，文件可能先被 lazy 创建
    // （空）再写入内容；只轮询"存在"会在 worker 调度延迟下（Windows / CI
    // 负载）拿到空文件而误判 flush 契约。把内容断言也纳入轮询，上限 5s
    // 兜底调度延迟，匹配即提前退出（happy path 仍是毫秒级）。
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut matched = false;
    let mut last_seen: Option<(std::path::PathBuf, String)> = None;
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(dir.path()) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("rss-ai-news.log.") {
                    let path = entry.path();
                    let body = std::fs::read_to_string(&path).unwrap_or_default();
                    matched = body.contains("rss-ai-news-test-marker-line");
                    last_seen = Some((path, body));
                    break;
                }
            }
        }
        if matched {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        matched,
        "rolling::daily 应在 dir 下创建 prefix.YYYY-MM-DD 且刚 emit 的 marker \
         已落盘；最后观测：{last_seen:?}"
    );
}
