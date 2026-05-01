use std::borrow::Cow;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use url::Url;

static AUTHZ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(authorization\s*:\s*)(bearer|basic|token)\s+\S+").unwrap());

static SECRET_KEY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)_(token|key|secret|password)$").unwrap());

pub fn redact_authorization_header(input: &str) -> Cow<'_, str> {
    AUTHZ_RE.replace_all(input, "$1$2 ***")
}

pub fn redact_url_userinfo(input: &str) -> Cow<'_, str> {
    if let Ok(mut url) = Url::parse(input)
        && (!url.username().is_empty() || url.password().is_some())
    {
        let _ = url.set_username("***");
        let _ = url.set_password(None);
        return Cow::Owned(url.to_string());
    }
    Cow::Borrowed(input)
}

pub fn redact_json_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if SECRET_KEY_RE.is_match(key) {
                    *value = Value::String("***".to_string());
                } else {
                    redact_json_secrets(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json_secrets(item);
            }
        }
        _ => {}
    }
}
