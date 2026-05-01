use rss_ai_news_observability::tracing_init::{InitOptions, init};

#[test]
fn tracing_init_is_idempotent() {
    init(InitOptions {
        log_level: "info".to_string(),
        log_format: "pretty".to_string(),
        log_file: String::new(),
    });
    init(InitOptions {
        log_level: "debug".to_string(),
        log_format: "json".to_string(),
        log_file: "doctor.log".to_string(),
    });
}
