use rss_ai_news_observability::redact::{
    redact_authorization_header, redact_json_secrets, redact_url_userinfo,
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
