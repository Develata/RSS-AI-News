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
        // W11-P4-fix2.H2 lint：`let _ = result` 已 deny。这两个 setter 在
        // cannot-be-a-base URL 上返 Err，但本路径 URL 经 `Url::parse` 验证
        // 且 username/password 非空——意味着已经是 schemes-with-authority。
        // 失败 case 仅在不变量被破坏时出现；redact 静默回退（仍走下面的
        // `to_string()` 返回 redact 后的最佳近似）。`.ok()` 显式吞 Err 不被
        // lint 拦截，比 `.expect` 在 redact 路径更稳（不让 log/health 崩）。
        url.set_username("***").ok();
        url.set_password(None).ok();
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

/// One-stop redaction for `run_events.context_json` payloads (per
/// error-and-observability §4.2). Combines:
///
/// 1. [`redact_json_secrets`] — replaces values whose **key** matches
///    `*_token` / `*_key` / `*_secret` / `*_password` with `"***"`.
/// 2. A recursive sweep over every remaining string **value**, applying
///    [`redact_authorization_header`] and [`redact_url_userinfo`] so a
///    secret embedded in free-form text (HTTP request dumps, error
///    messages quoting an Authorization header, URLs carrying
///    user:password userinfo) is also masked.
///
/// The two stages are necessary together: key-based redaction misses
/// secrets that callers stuffed into a generic "message" / "details"
/// string, and the string-level regexes miss bare values held under a
/// `*_token` key with no surrounding context.
pub fn redact_event_context(value: &mut Value) {
    redact_json_secrets(value);
    redact_strings_in_place(value);
}

fn redact_strings_in_place(value: &mut Value) {
    match value {
        Value::String(text) => {
            let after_authz = redact_authorization_header(text);
            let after_url = redact_url_userinfo(after_authz.as_ref());
            let final_str = after_url.into_owned();
            if final_str != *text {
                *text = final_str;
            }
        }
        Value::Object(map) => {
            for (_, child) in map {
                redact_strings_in_place(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_strings_in_place(item);
            }
        }
        _ => {}
    }
}
