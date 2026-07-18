const REDACTED: &str = "[REDACTED]";
const SENSITIVE_KEYS: [&str; 14] = [
    "database_url",
    "password",
    "passwd",
    "token",
    "access_token",
    "secret",
    "client_secret",
    "secret_key",
    "api_key",
    "access_key",
    "access_key_id",
    "private_key",
    "authorization",
    "credential",
];

pub(crate) fn is_sensitive_env_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    SENSITIVE_KEYS.iter().any(|marker| {
        normalized == *marker
            || normalized.ends_with(&format!("_{marker}"))
            || normalized.starts_with(&format!("{marker}_"))
    }) || (normalized.ends_with("_url")
        && ["database", "postgres", "mysql", "redis", "amqp"]
            .iter()
            .any(|marker| normalized.contains(marker)))
}

pub(crate) fn redact_output<'a>(
    input: &str,
    secret_values: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut redacted = input.to_string();
    for secret in secret_values {
        if !secret.is_empty() && !secret.starts_with('$') {
            redacted = redacted.replace(secret, REDACTED);
        }
    }
    redacted = redact_url_userinfo(redacted);
    for key in SENSITIVE_KEYS {
        redacted = redact_assignments(redacted, key);
    }
    redact_bearer_tokens(redacted)
}

fn redact_url_userinfo(mut value: String) -> String {
    let mut cursor = 0;
    while let Some(relative_scheme) = value[cursor..].find("://") {
        let authority_start = cursor + relative_scheme + 3;
        let authority_end = value[authority_start..]
            .find(|ch: char| ch.is_whitespace() || "'\"<>".contains(ch))
            .map_or(value.len(), |offset| authority_start + offset);
        let Some(relative_at) = value[authority_start..authority_end].find('@') else {
            cursor = authority_end;
            continue;
        };
        let at = authority_start + relative_at;
        value.replace_range(authority_start..at, REDACTED);
        cursor = authority_start + REDACTED.len() + 1;
    }
    value
}

fn redact_assignments(mut value: String, key: &str) -> String {
    let mut cursor = 0;
    loop {
        let lower = value.to_ascii_lowercase();
        let Some(relative_key) = lower[cursor..].find(key) else {
            break;
        };
        let key_start = cursor + relative_key;
        let key_end = key_start + key.len();
        if !is_key_boundary(&lower, key_start, key_end) {
            cursor = key_end;
            continue;
        }

        let bytes = value.as_bytes();
        let mut separator = key_end;
        if bytes.get(separator) == Some(&b'"') || bytes.get(separator) == Some(&b'\'') {
            separator += 1;
        }
        while bytes.get(separator).is_some_and(u8::is_ascii_whitespace) {
            separator += 1;
        }
        if !matches!(bytes.get(separator), Some(b'=') | Some(b':')) {
            cursor = key_end;
            continue;
        }
        separator += 1;
        while bytes.get(separator).is_some_and(u8::is_ascii_whitespace) {
            separator += 1;
        }
        let quote = bytes
            .get(separator)
            .copied()
            .filter(|byte| matches!(byte, b'"' | b'\''));
        let value_start = separator + usize::from(quote.is_some());
        let value_end = find_value_end(&value, value_start, quote);
        if value_start >= value_end {
            cursor = value_end;
            continue;
        }
        value.replace_range(value_start..value_end, REDACTED);
        cursor = value_start + REDACTED.len();
    }
    value
}

fn is_key_boundary(value: &str, start: usize, end: usize) -> bool {
    let before = value[..start].chars().next_back();
    let after = value[end..].chars().next();
    !before.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !after.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn find_value_end(value: &str, start: usize, quote: Option<u8>) -> usize {
    let bytes = value.as_bytes();
    let mut end = start;
    while let Some(byte) = bytes.get(end) {
        if quote == Some(*byte)
            || (quote.is_none()
                && (byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'}' | b']')))
        {
            break;
        }
        end += 1;
    }
    end
}

fn redact_bearer_tokens(mut value: String) -> String {
    let mut cursor = 0;
    loop {
        let lower = value.to_ascii_lowercase();
        let Some(relative) = lower[cursor..].find("bearer ") else {
            break;
        };
        let token_start = cursor + relative + "bearer ".len();
        let token_end = value[token_start..]
            .find(|ch: char| ch.is_whitespace() || "'\",;}".contains(ch))
            .map_or(value.len(), |offset| token_start + offset);
        if token_start == token_end {
            cursor = token_end;
            continue;
        }
        value.replace_range(token_start..token_end, REDACTED);
        cursor = token_start + REDACTED.len();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{REDACTED, is_sensitive_env_key, redact_output};

    #[test]
    fn redacts_exact_environment_secret_and_url_userinfo() {
        let database_url = "postgres://alice:hunter2@db.example.test/rss";
        let output = redact_output(
            &format!("DATABASE_URL={database_url}\nretry https://bob:pw@example.test/path"),
            [database_url],
        );
        assert!(!output.contains("hunter2"));
        assert!(!output.contains("bob:pw"));
        assert!(output.contains(REDACTED));
    }

    #[test]
    fn redacts_json_assignments_and_bearer_tokens() {
        let output = redact_output(
            r#"{"api_key":"sk-live-value","error":"Bearer abc.def.ghi"}"#,
            [],
        );
        assert!(!output.contains("sk-live-value"));
        assert!(!output.contains("abc.def.ghi"));
        assert!(output.matches(REDACTED).count() >= 2);
    }

    #[test]
    fn does_not_redact_unassigned_sensitive_words() {
        let input = "token budget and password policy";
        assert_eq!(redact_output(input, []), input);
    }

    #[test]
    fn classifies_sensitive_environment_names_without_matching_normal_runtime_keys() {
        for key in [
            "OPENAI_API_KEY",
            "GH_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_ACCESS_KEY_ID",
            "DATABASE_URL",
            "POSTGRES_URL",
        ] {
            assert!(is_sensitive_env_key(key), "expected sensitive key: {key}");
        }
        for key in ["PATH", "HOME", "RUST_LOG", "TOKENIZERS_PARALLELISM"] {
            assert!(
                !is_sensitive_env_key(key),
                "unexpected sensitive key: {key}"
            );
        }
    }
}
