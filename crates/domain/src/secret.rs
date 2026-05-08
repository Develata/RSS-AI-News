//! Secret-redacting newtype for sensitive strings.
//!
//! Wraps an inner `String` so that `Debug`, `Display`, and JSON serialization
//! never leak the underlying value into logs or error messages. Callers must
//! explicitly call [`SecretString::expose_secret`] to retrieve the raw value
//! at the use site (HTTP `Authorization` headers, etc.).
//!
//! See `docs/handoffs/2026-05-07-w0-doc-freeze-e2-decisions.md` Issue 4 for
//! the freeze contract and `docs/design/config-schema.md` for env wiring.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Newtype that hides its inner string from `Debug`/`Display`/`Serialize`.
///
/// `expose_secret` is the only way to read the underlying value, making
/// accidental leakage via `format!("{:?}", env)` impossible.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

const REDACTED: &str = "***";

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretString").field(&REDACTED).finish()
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl Serialize for SecretString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(REDACTED)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "sk-extremely-secret-token-1234567890";

    #[test]
    fn debug_does_not_leak_inner_value() {
        let secret = SecretString::new(RAW);
        let rendered = format!("{secret:?}");
        assert!(rendered.contains(REDACTED));
        assert!(!rendered.contains(RAW));
    }

    #[test]
    fn display_does_not_leak_inner_value() {
        let secret = SecretString::new(RAW);
        let rendered = format!("{secret}");
        assert_eq!(rendered, REDACTED);
        assert!(!rendered.contains(RAW));
    }

    #[test]
    fn expose_secret_returns_original_value() {
        let secret = SecretString::new(RAW);
        assert_eq!(secret.expose_secret(), RAW);
    }

    #[test]
    fn serialize_redacts_value_to_prevent_log_leakage() {
        let secret = SecretString::new(RAW);
        let json = serde_json::to_string(&secret).expect("serialize");
        assert_eq!(json, format!("\"{REDACTED}\""));
        assert!(!json.contains(RAW));
    }

    #[test]
    fn deserialize_accepts_plain_string() {
        let json = format!("\"{RAW}\"");
        let secret: SecretString = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(secret.expose_secret(), RAW);
    }

    #[test]
    fn from_string_and_str_constructs_equivalent_value() {
        let from_owned: SecretString = String::from(RAW).into();
        let from_borrowed: SecretString = RAW.into();
        assert_eq!(from_owned, from_borrowed);
        assert_eq!(from_owned.expose_secret(), RAW);
    }

    #[test]
    fn debug_in_struct_context_redacts_value() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            token: SecretString,
            note: &'static str,
        }
        let holder = Holder {
            token: SecretString::new(RAW),
            note: "visible",
        };
        let rendered = format!("{holder:?}");
        assert!(rendered.contains("visible"));
        assert!(rendered.contains(REDACTED));
        assert!(!rendered.contains(RAW));
    }
}
