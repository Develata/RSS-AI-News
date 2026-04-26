//! URL normalization for feed entry deduplication.

use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::error::ClassifiedError;

const TRACKING_QUERY_KEYS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "fbclid",
    "gclid",
    "mc_cid",
    "mc_eid",
    "igshid",
    "ref_src",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedLink {
    pub normalized: String,
    pub link_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LinkNormalizeError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),
}

pub fn normalize_link(raw: &str) -> Result<NormalizedLink, LinkNormalizeError> {
    let mut url = Url::parse(raw).map_err(|_| LinkNormalizeError::InvalidUrl(raw.to_string()))?;

    match url.scheme() {
        "http" | "https" => {}
        scheme => return Err(LinkNormalizeError::UnsupportedScheme(scheme.to_string())),
    }

    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);

    normalize_path(&mut url);
    normalize_query(&mut url);

    let normalized = url.to_string();
    let link_hash = sha256_hex(normalized.as_bytes());

    Ok(NormalizedLink {
        normalized,
        link_hash,
    })
}

fn normalize_path(url: &mut Url) {
    let path = url.path().to_string();
    if path != "/" {
        let trimmed = path.trim_end_matches('/').to_string();
        if trimmed.is_empty() {
            url.set_path("/");
        } else if trimmed != path {
            url.set_path(&trimmed);
        }
    }
}

fn normalize_query(url: &mut Url) {
    let Some(_) = url.query() else {
        return;
    };

    let mut pairs = url
        .query_pairs()
        .filter(|(key, _)| !TRACKING_QUERY_KEYS.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();

    pairs.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    url.set_query(None);
    if !pairs.is_empty() {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(&key, &value);
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

impl ClassifiedError for LinkNormalizeError {
    fn is_retryable(&self) -> bool {
        false
    }

    fn error_kind(&self) -> &str {
        match self {
            Self::InvalidUrl(_) => "invalid_url",
            Self::UnsupportedScheme(_) => "unsupported_scheme",
        }
    }

    fn display_user(&self) -> String {
        match self {
            Self::InvalidUrl(_) => "链接不是合法 URL".to_string(),
            Self::UnsupportedScheme(_) => "链接协议不受支持".to_string(),
        }
    }

    fn display_debug(&self) -> String {
        match self {
            Self::InvalidUrl(url) => format!("invalid URL: {url}"),
            Self::UnsupportedScheme(scheme) => format!("unsupported URL scheme: {scheme}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(raw: &str) -> String {
        normalize_link(raw)
            .expect("link should normalize")
            .normalized
    }

    #[test]
    fn normalizes_scheme_and_host_case() {
        assert_eq!(
            normalized("HTTPS://Example.COM/foo"),
            "https://example.com/foo"
        );
    }

    #[test]
    fn removes_default_port() {
        assert_eq!(
            normalized("https://example.com:443/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn removes_fragment() {
        assert_eq!(normalized("https://x.com/a#frag"), "https://x.com/a");
    }

    #[test]
    fn removes_tracking_query_keys() {
        assert_eq!(
            normalized("https://x.com/a?utm_source=x&id=1"),
            "https://x.com/a?id=1"
        );
    }

    #[test]
    fn sorts_remaining_query_pairs() {
        assert_eq!(
            normalized("https://x.com/a?b=2&a=1"),
            "https://x.com/a?a=1&b=2"
        );
    }

    #[test]
    fn removes_trailing_slash_except_root() {
        assert_eq!(normalized("https://x.com/a/"), "https://x.com/a");
        assert_eq!(normalized("https://x.com/"), "https://x.com/");
    }

    #[test]
    fn removes_userinfo() {
        assert_eq!(
            normalized("https://user:pass@example.com/a"),
            "https://example.com/a"
        );
    }

    #[test]
    fn rejects_ftp_scheme() {
        let err = normalize_link("ftp://x.com/y").expect_err("ftp should be rejected");
        assert_eq!(
            err,
            LinkNormalizeError::UnsupportedScheme("ftp".to_string())
        );
        assert!(!err.is_retryable());
        assert_eq!(err.error_kind(), "unsupported_scheme");
    }

    #[test]
    fn rejects_non_url() {
        let err = normalize_link("not-a-url").expect_err("invalid URL should be rejected");
        assert!(matches!(err, LinkNormalizeError::InvalidUrl(_)));
        assert!(!err.is_retryable());
        assert_eq!(err.error_kind(), "invalid_url");
    }

    #[test]
    fn link_hash_is_lowercase_sha256_hex() {
        let link = normalize_link("https://example.com/x").expect("link should normalize");
        assert_eq!(link.link_hash.len(), 64);
        assert!(link.link_hash.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(
            link.link_hash
                .chars()
                .all(|ch| !ch.is_ascii_alphabetic() || ch.is_ascii_lowercase())
        );
    }

    #[test]
    fn same_normalized_string_has_stable_hash() {
        let left = normalize_link("https://example.com/x?b=2&a=1").expect("left should normalize");
        let right =
            normalize_link("https://example.com/x?a=1&b=2#frag").expect("right should normalize");

        assert_eq!(left.normalized, right.normalized);
        assert_eq!(left.link_hash, right.link_hash);
    }
}
