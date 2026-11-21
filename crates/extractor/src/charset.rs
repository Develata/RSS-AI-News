//! HTML byte → text decoding (charset detection).
//!
//! Candidate order follows the WHATWG encoding sniffing algorithm (HTML §13.2.3,
//! simplified): byte order mark → `Content-Type` charset parameter → `<meta
//! charset>` / `<meta http-equiv="Content-Type" content="…charset=…">` in the
//! first 1024 bytes → UTF-8. Deviations, chosen so that a wrong guess never
//! publishes mojibake:
//! - every candidate is decoded strictly (no U+FFFD replacement); a candidate
//!   whose decode hits a malformed sequence is skipped and the next one tried;
//! - there is no locale-dependent fallback (browsers guess windows-1252 for
//!   undeclared non-UTF-8 bytes); if no candidate decodes cleanly the page is
//!   rejected with [`ExtractorError::ParseFailed`], as before this module;
//! - the prescan skips comments but does not tokenize attributes fully.

use std::borrow::Cow;

use encoding_rs::{Encoding, UTF_8};

use crate::error::ExtractorError;

/// Bytes of the document head searched for a `<meta>` charset declaration.
const PRESCAN_LIMIT: usize = 1024;

/// Decode fetched HTML bytes to text.
///
/// `content_type` is the response's `Content-Type` header value, if any.
/// Returns borrowed text when the bytes are already valid UTF-8 without BOM
/// and no other declaration wins.
///
/// Errors: [`ExtractorError::ParseFailed`] when no candidate (BOM, header,
/// meta, UTF-8) decodes the bytes without malformed sequences.
pub fn decode_html<'a>(
    bytes: &'a [u8],
    content_type: Option<&str>,
) -> Result<Cow<'a, str>, ExtractorError> {
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes)
        && let Some(text) = decode_strict(encoding, &bytes[bom_len..])
    {
        return Ok(text);
    }

    let mut tried: Vec<&'static Encoding> = Vec::new();
    let declared = [
        content_type
            .and_then(charset_param)
            .and_then(|label| Encoding::for_label(label.as_bytes())),
        // A meta-declared UTF-16 label means UTF-8 (WHATWG); `output_encoding`
        // applies that mapping and leaves every other encoding unchanged.
        prescan_meta_charset(&bytes[..bytes.len().min(PRESCAN_LIMIT)])
            .map(Encoding::output_encoding),
    ];
    for encoding in declared.into_iter().flatten() {
        if tried.contains(&encoding) {
            continue;
        }
        tried.push(encoding);
        if let Some(text) = decode_strict(encoding, bytes) {
            return Ok(text);
        }
    }

    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(Cow::Borrowed(text));
    }

    let declared_names = tried
        .iter()
        .map(|encoding| encoding.name())
        .collect::<Vec<_>>();
    Err(ExtractorError::ParseFailed {
        reason: if declared_names.is_empty() {
            "HTML is not valid UTF-8 and declares no charset".to_string()
        } else {
            format!(
                "HTML does not decode as its declared charset ({}) or UTF-8",
                declared_names.join(", ")
            )
        },
    })
}

/// Strict decode without BOM sniffing; `None` on any malformed sequence or
/// for the WHATWG "replacement" encoding (which decodes everything to U+FFFD).
fn decode_strict<'a>(encoding: &'static Encoding, bytes: &'a [u8]) -> Option<Cow<'a, str>> {
    if encoding == encoding_rs::REPLACEMENT {
        return None;
    }
    if encoding == UTF_8 {
        return std::str::from_utf8(bytes).ok().map(Cow::Borrowed);
    }
    encoding.decode_without_bom_handling_and_without_replacement(bytes)
}

/// `charset` parameter of a `Content-Type` value (case-insensitive name,
/// optional quotes).
fn charset_param(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        (!value.is_empty()).then_some(value)
    })
}

/// Simplified WHATWG prescan: the first `<meta>` tag carrying a `charset`
/// attribute, or an `http-equiv="content-type"` with a `charset=` in its
/// content, whose label names a known encoding. Comments are skipped.
fn prescan_meta_charset(head: &[u8]) -> Option<&'static Encoding> {
    let mut pos = 0;
    while pos < head.len() {
        let rest = &head[pos..];
        if rest.starts_with(b"<!--") {
            pos += find(&rest[4..], b"-->").map_or(rest.len(), |end| end + 4 + 3);
            continue;
        }
        if rest.len() >= 6
            && rest[0] == b'<'
            && rest[1..5].eq_ignore_ascii_case(b"meta")
            && matches!(rest[5], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/')
        {
            let end = rest.iter().position(|&b| b == b'>').unwrap_or(rest.len());
            let tag = rest[5..end].to_ascii_lowercase();
            if let Some(encoding) = meta_tag_charset(&tag) {
                return Some(encoding);
            }
            pos += end;
            continue;
        }
        pos += 1;
    }
    None
}

/// Charset of one lowercased `<meta …>` attribute string, if it declares one.
fn meta_tag_charset(tag: &[u8]) -> Option<&'static Encoding> {
    if let Some(value) = attribute_value(tag, b"charset") {
        return Encoding::for_label(value);
    }
    let http_equiv = attribute_value(tag, b"http-equiv")?;
    if http_equiv != b"content-type" {
        return None;
    }
    let content = attribute_value(tag, b"content")?;
    let at = find(content, b"charset")?;
    let after = content[at + b"charset".len()..].trim_ascii_start();
    let after = after.strip_prefix(b"=")?.trim_ascii_start();
    let after = after
        .strip_prefix(b"\"")
        .or_else(|| after.strip_prefix(b"'"))
        .unwrap_or(after);
    let end = after
        .iter()
        .position(|&b| matches!(b, b'"' | b'\'' | b';' | b' ' | b'\t' | b'\n' | b'\r'))
        .unwrap_or(after.len());
    Encoding::for_label(&after[..end])
}

/// Value of attribute `name` in a lowercased attribute string, parsed as
/// `name[=value]` pairs whose values may be quoted, so text inside another
/// attribute's quoted value (`content="…charset=…"`) never matches.
fn attribute_value<'t>(tag: &'t [u8], name: &[u8]) -> Option<&'t [u8]> {
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c');
    let mut pos = 0;
    while pos < tag.len() {
        while pos < tag.len() && (is_space(tag[pos]) || tag[pos] == b'/') {
            pos += 1;
        }
        let name_start = pos;
        while pos < tag.len() && !is_space(tag[pos]) && !matches!(tag[pos], b'=' | b'/') {
            pos += 1;
        }
        let attr_name = &tag[name_start..pos];
        while pos < tag.len() && is_space(tag[pos]) {
            pos += 1;
        }
        let mut value: &[u8] = b"";
        if pos < tag.len() && tag[pos] == b'=' {
            pos += 1;
            while pos < tag.len() && is_space(tag[pos]) {
                pos += 1;
            }
            match tag.get(pos) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let start = pos + 1;
                    let end = tag[start..]
                        .iter()
                        .position(|&b| b == quote)
                        .map_or(tag.len(), |offset| start + offset);
                    value = &tag[start..end];
                    pos = (end + 1).min(tag.len());
                }
                _ => {
                    let start = pos;
                    while pos < tag.len() && !is_space(tag[pos]) {
                        pos += 1;
                    }
                    value = &tag[start..pos];
                }
            }
        }
        if attr_name.is_empty() {
            // A stray `=`: skip it so the loop always advances.
            pos += 1;
            continue;
        }
        if attr_name == name {
            return Some(value.trim_ascii());
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charset_param_is_case_insensitive_and_unquoted() {
        assert_eq!(charset_param("text/html; Charset=\"GBK\""), Some("GBK"));
        assert_eq!(charset_param("text/html;charset=utf-8"), Some("utf-8"));
        assert_eq!(charset_param("text/html"), None);
        assert_eq!(charset_param("text/html; charset="), None);
    }

    #[test]
    fn prescan_reads_both_meta_forms_and_skips_comments() {
        assert_eq!(
            prescan_meta_charset(b"<html><head><META CHARSET=gb2312>"),
            Some(encoding_rs::GBK)
        );
        assert_eq!(
            prescan_meta_charset(
                b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=Shift_JIS\">"
            ),
            Some(encoding_rs::SHIFT_JIS)
        );
        assert_eq!(
            prescan_meta_charset(b"<!-- <meta charset=big5> --><meta charset=\"euc-kr\">"),
            Some(encoding_rs::EUC_KR)
        );
        assert_eq!(
            prescan_meta_charset(b"<meta name=\"description\" content=\"charset=gbk\">"),
            None
        );
        assert_eq!(prescan_meta_charset(b"<metadata charset=gbk>"), None);
    }

    #[test]
    fn meta_beyond_the_first_1024_bytes_is_ignored() {
        let mut html = vec![b' '; PRESCAN_LIMIT];
        html.extend_from_slice(b"<meta charset=gbk>\xd6\xd0");
        assert!(decode_html(&html, None).is_err());
    }

    #[test]
    fn utf16_meta_label_means_utf8() {
        let html = "<meta charset=utf-16><p>中文</p>";
        assert_eq!(decode_html(html.as_bytes(), None).unwrap(), html);
    }

    #[test]
    fn utf16_bom_decodes_as_utf16() {
        let mut html = vec![0xFF, 0xFE];
        for unit in "<p>中文</p>".encode_utf16() {
            html.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_html(&html, None).unwrap(), "<p>中文</p>");
    }

    #[test]
    fn bom_wins_over_declarations() {
        let mut html = vec![0xEF, 0xBB, 0xBF];
        html.extend_from_slice("<meta charset=gbk><p>中文</p>".as_bytes());
        assert_eq!(
            decode_html(&html, Some("text/html; charset=gbk")).unwrap(),
            "<meta charset=gbk><p>中文</p>"
        );
    }

    #[test]
    fn declared_charset_that_does_not_decode_falls_through_to_the_next() {
        // Header claims UTF-8, bytes are GBK, meta says GBK.
        let mut html = b"<meta charset=gbk><p>".to_vec();
        html.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]);
        assert_eq!(
            decode_html(&html, Some("text/html; charset=utf-8")).unwrap(),
            "<meta charset=gbk><p>中文"
        );
    }
}
