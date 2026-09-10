use std::borrow::Cow;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use url::Url;

static AUTHZ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(authorization\s*:\s*)(bearer|basic|token)\s+\S+").unwrap());

static SECRET_KEY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(^|[_-])(token|key|secret|password|signature|credential)$|^(authorization|cookie|set-cookie|sig|auth)$")
        .expect("static secret-key pattern")
});

static URL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s<>"']+"#).expect("static URL pattern")
});

pub fn redact_authorization_header(input: &str) -> Cow<'_, str> {
    AUTHZ_RE.replace_all(input, "$1$2 ***")
}

/// Redact URL credentials and sensitive query values, including URLs embedded
/// in error messages. Preserve the original text and borrowing when unchanged.
pub fn redact_url_userinfo(input: &str) -> Cow<'_, str> {
    let mut output: Option<String> = None;
    let mut copied_until = 0;
    for matched in URL_RE.find_iter(input) {
        let raw = matched.as_str();
        let Ok(mut url) = Url::parse(raw) else {
            // A malformed credential-bearing URL must not bypass redaction.
            let result = output.get_or_insert_with(|| String::with_capacity(input.len()));
            result.push_str(&input[copied_until..matched.start()]);
            result.push_str("[invalid URL]");
            copied_until = matched.end();
            continue;
        };
        let has_userinfo = !url.username().is_empty() || url.password().is_some();
        let has_secret_query = url
            .query_pairs()
            .any(|(key, _)| SECRET_KEY_RE.is_match(&key));
        if !has_userinfo && !has_secret_query {
            continue;
        }
        if has_userinfo && (url.set_username("***").is_err() || url.set_password(None).is_err()) {
            // Fail closed if URL invariants ever change.
            let result = output.get_or_insert_with(|| String::with_capacity(input.len()));
            result.push_str(&input[copied_until..matched.start()]);
            result.push_str("[redacted URL]");
            copied_until = matched.end();
            continue;
        }
        if has_secret_query {
            let query = url.query().unwrap_or_default().to_owned();
            url.query_pairs_mut().clear().extend_pairs(
                url::form_urlencoded::parse(query.as_bytes()).map(|(key, value)| {
                    let value = if SECRET_KEY_RE.is_match(&key) {
                        Cow::Borrowed("***")
                    } else {
                        value
                    };
                    (key, value)
                }),
            );
        }
        let result = output.get_or_insert_with(|| String::with_capacity(input.len()));
        result.push_str(&input[copied_until..matched.start()]);
        result.push_str(url.as_str());
        copied_until = matched.end();
    }
    match output {
        Some(mut output) => {
            output.push_str(&input[copied_until..]);
            Cow::Owned(output)
        }
        None => Cow::Borrowed(input),
    }
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
/// 1. [`redact_json_secrets`] — masks credential keys (bare names and suffixes),
///    including token, key, secret, password, signature and Authorization.
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
            let replacement = if after_url != *text {
                Some(after_url.into_owned())
            } else {
                None
            };
            if let Some(replacement) = replacement {
                *text = replacement;
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
