use rss_ai_news_observability::redact::{
    redact_authorization_header, redact_event_context, redact_json_secrets, redact_url_userinfo,
};
use serde_json::json;

#[test]
fn redact_authorization_header_removes_bearer_value() {
    let secret = "sk-test-secret-token";
    let input = format!("Authorization: Bearer {secret}");
    let redacted = redact_authorization_header(&input);
    assert!(redacted.contains("Bearer ***"));
    assert!(!redacted.contains(secret));
}

#[test]
fn redact_url_userinfo_removes_username_and_password() {
    let redacted = redact_url_userinfo("https://user:pass@example.com/path");
    assert_eq!(redacted, "https://***@example.com/path");
    assert!(!redacted.contains("user"));
    assert!(!redacted.contains("pass"));
}

#[test]
fn redact_json_secrets_removes_sensitive_suffix_values() {
    let secret = "AKIA1234567890ABCDEF";
    let mut value = json!({"openai_api_key": secret, "note": "fine"});
    redact_json_secrets(&mut value);
    let text = value.to_string();
    assert!(!text.contains(secret));
    assert!(text.contains("fine"));
}

#[test]
fn redact_json_secrets_handles_nested_values_without_false_positive() {
    let secret = "github-token-proof";
    let mut value = json!({
        "outer": [{"github_token": secret}],
        "keylogger_research": "safe"
    });
    redact_json_secrets(&mut value);
    let text = value.to_string();
    assert!(!text.contains(secret));
    assert!(text.contains("safe"));
}

#[test]
fn redact_event_context_combines_key_and_string_redaction() {
    // §4.2 contract: a single function call must mask both
    // key-based secrets (e.g. `*_token`) and secrets embedded in
    // free-form string values (Authorization headers, URL userinfo).
    let key_secret = "key-secret-aaa";
    let header_secret = "header-secret-bbb";
    let url_password = "url-password-ccc";
    let mut value = json!({
        "openai_api_key": key_secret,
        "request_dump": format!("Authorization: Bearer {header_secret}"),
        "callback": format!("https://operator:{url_password}@hooks.test/path"),
        "note": "fine"
    });
    redact_event_context(&mut value);
    let text = value.to_string();
    assert!(!text.contains(key_secret));
    assert!(!text.contains(header_secret));
    assert!(!text.contains(url_password));
    assert!(text.contains("Bearer ***"));
    assert!(text.contains("***@hooks.test"));
    assert!(text.contains("fine"));
}

#[test]
fn redact_event_context_preserves_non_secret_string_values_unchanged() {
    let mut value = json!({
        "step": "ingest",
        "count": 42,
        "url": "https://api.example.test/v1/articles"
    });
    redact_event_context(&mut value);
    assert_eq!(value["step"], json!("ingest"));
    assert_eq!(value["count"], json!(42));
    assert_eq!(value["url"], json!("https://api.example.test/v1/articles"));
}
