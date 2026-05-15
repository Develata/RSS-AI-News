use rss_ai_news_observability::tracing_init::{InitOptions, init};

#[test]
fn tracing_init_is_idempotent_and_file_mode_does_not_panic() {
    // F15-13 W9-F1：init 第一次安装订阅者；后续调用要么走 stderr 路径、
    // 要么 spawn 一个被即时 drop 的 file appender（worker 在 channel 关闭
    // 时退出，无资源泄漏）。本测试只验证不 panic + 第一次返 guard 的契约。
    let first = init(InitOptions {
        log_level: "info".to_string(),
        log_format: "pretty".to_string(),
        log_file: String::new(),
    });
    // 第一次走 stderr 路径，没有 guard 可返。
    assert!(first.is_none(), "stderr 模式不应返回 WorkerGuard");

    // 第二次：用 tempdir 防止污染工作目录；try_init 失败 → 同样返回 None。
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("doctor.log");
    let second = init(InitOptions {
        log_level: "debug".to_string(),
        log_format: "json".to_string(),
        log_file: log_path.to_string_lossy().into_owned(),
    });
    assert!(
        second.is_none(),
        "第二次 init 已无法安装订阅者，不应返回 guard"
    );
}
