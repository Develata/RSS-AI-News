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
//! - the prescan follows the spec's tag / attribute consumption (R2-S8), so
//!   attribute values, quoted `>` and the 1024-byte boundary are handled as a
//!   browser does; it does not implement the later "changing the encoding
//!   while parsing" step.

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
        // The prescan applies the meta fix-ups (UTF-16 → UTF-8,
        // x-user-defined → windows-1252).
        prescan_meta_charset(&bytes[..bytes.len().min(PRESCAN_LIMIT)]),
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

    // Implicit UTF-8 last; its failure is reported as ParseFailed below.
    if let Some(text) = decode_strict(UTF_8, bytes) {
        return Ok(text);
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

/// `charset` parameter of a `Content-Type` value (RFC 9110 §5.6.6: name
/// case-insensitive, value a token or a quoted-string with `\` escapes).
/// Parameters are split respecting quoted strings, so a `;charset=` inside
/// another parameter's quoted value is not a charset (R2-S8).
fn charset_param(content_type: &str) -> Option<String> {
    let bytes = content_type.as_bytes();
    // Skip the media type.
    let mut pos = bytes.iter().position(|&b| b == b';')? + 1;
    while pos < bytes.len() {
        while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t' | b';') {
            pos += 1;
        }
        let name_start = pos;
        while pos < bytes.len() && !matches!(bytes[pos], b'=' | b';') {
            pos += 1;
        }
        let name = content_type[name_start..pos].trim();
        if bytes.get(pos) != Some(&b'=') {
            continue;
        }
        pos += 1;
        while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
            pos += 1;
        }
        let mut value = String::new();
        if bytes.get(pos) == Some(&b'"') {
            pos += 1;
            let mut chars = content_type[pos..].char_indices();
            let mut closed = false;
            while let Some((offset, ch)) = chars.next() {
                match ch {
                    '"' => {
                        pos += offset + 1;
                        closed = true;
                        break;
                    }
                    '\\' => {
                        if let Some((_, escaped)) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    other => value.push(other),
                }
            }
            if !closed {
                pos = bytes.len();
            }
            // Anything up to the next `;` after a quoted string is ignored.
            while pos < bytes.len() && bytes[pos] != b';' {
                pos += 1;
            }
        } else {
            let start = pos;
            while pos < bytes.len() && bytes[pos] != b';' {
                pos += 1;
            }
            value.push_str(content_type[start..pos].trim());
        }
        if name.eq_ignore_ascii_case("charset") {
            let value = value.trim().to_string();
            return (!value.is_empty()).then_some(value);
        }
    }
    None
}

fn is_html_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// WHATWG "prescan a byte stream to determine its encoding" (HTML §13.2.3.2)
/// over `head` (the first 1024 bytes). Every tag's attributes are consumed
/// with the spec's attribute algorithm, so `<meta` text inside another tag's
/// quoted attribute value and a quoted `>` never confuse it; comments,
/// `<!…>`, `</…>` and `<?…>` are skipped. Running off the end of `head` in
/// the middle of a tag aborts the prescan: a declaration not complete within
/// the first 1024 bytes does not count (R2-S8).
fn prescan_meta_charset(head: &[u8]) -> Option<&'static Encoding> {
    let mut pos = 0;
    while pos < head.len() {
        let rest = &head[pos..];
        if rest.starts_with(b"<!--") {
            // The `--` of `-->` may overlap the opening `<!--` (`<!-->`).
            pos += 2 + find(&rest[2..], b"-->")? + 3;
            continue;
        }
        if rest.len() >= 6
            && rest[0] == b'<'
            && rest[1..5].eq_ignore_ascii_case(b"meta")
            && (is_html_space(rest[5]) || rest[5] == b'/')
        {
            pos += 6;
            if let Some(encoding) = prescan_meta_tag(head, &mut pos)? {
                return Some(encoding);
            }
            continue;
        }
        let tag_start = match rest {
            [b'<', b'/', letter, ..] if letter.is_ascii_alphabetic() => Some(2),
            [b'<', letter, ..] if letter.is_ascii_alphabetic() => Some(1),
            _ => None,
        };
        if let Some(skip) = tag_start {
            pos += skip;
            while pos < head.len() && !is_html_space(head[pos]) && head[pos] != b'>' {
                pos += 1;
            }
            while next_attribute(head, &mut pos)?.is_some() {}
            continue;
        }
        if rest.starts_with(b"<!") || rest.starts_with(b"</") || rest.starts_with(b"<?") {
            pos += rest.iter().position(|&b| b == b'>')? + 1;
            continue;
        }
        pos += 1;
    }
    None
}

/// The attributes of one `<meta` tag (`pos` just after `<meta` + one byte).
/// `None`: the input ended inside the tag (abort). `Some(None)`: no usable
/// declaration, the scan continues after the tag.
fn prescan_meta_tag(head: &[u8], pos: &mut usize) -> Option<Option<&'static Encoding>> {
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut got_pragma = false;
    let mut need_pragma: Option<bool> = None;
    // `Some(None)` = a charset attribute named an unknown label (failure).
    let mut charset: Option<Option<&'static Encoding>> = None;
    while let Some((name, value)) = next_attribute(head, pos)? {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        match name.as_slice() {
            b"http-equiv" if value.eq_ignore_ascii_case(b"content-type") => got_pragma = true,
            b"content" if charset.is_none() => {
                if let Some(label) = charset_from_meta_content(&value) {
                    charset = Some(Encoding::for_label(label));
                    need_pragma = Some(true);
                }
            }
            b"charset" => {
                charset = Some(Encoding::for_label(&value));
                need_pragma = Some(false);
            }
            _ => {}
        }
    }
    let declared = match need_pragma {
        None => None,
        Some(true) if !got_pragma => None,
        _ => charset.flatten(),
    };
    Some(declared.map(meta_encoding))
}

/// WHATWG meta-declared encoding fix-ups: UTF-16 means UTF-8,
/// `x-user-defined` means windows-1252.
fn meta_encoding(encoding: &'static Encoding) -> &'static Encoding {
    if encoding == encoding_rs::X_USER_DEFINED {
        encoding_rs::WINDOWS_1252
    } else {
        encoding.output_encoding()
    }
}

/// WHATWG "get an attribute": `None` when the input ends first (abort),
/// `Some(None)` at the tag's `>`, else the lowercased name and raw value.
#[allow(clippy::type_complexity)]
fn next_attribute(head: &[u8], pos: &mut usize) -> Option<Option<(Vec<u8>, Vec<u8>)>> {
    while is_html_space(*head.get(*pos)?) || head[*pos] == b'/' {
        *pos += 1;
    }
    if head[*pos] == b'>' {
        *pos += 1;
        return Some(None);
    }
    let mut name = Vec::new();
    loop {
        let b = *head.get(*pos)?;
        if b == b'=' && !name.is_empty() {
            *pos += 1;
            break;
        }
        if is_html_space(b) {
            while is_html_space(*head.get(*pos)?) {
                *pos += 1;
            }
            if head[*pos] != b'=' {
                return Some(Some((name, Vec::new())));
            }
            *pos += 1;
            break;
        }
        if b == b'/' || b == b'>' {
            return Some(Some((name, Vec::new())));
        }
        name.push(b.to_ascii_lowercase());
        *pos += 1;
    }
    while is_html_space(*head.get(*pos)?) {
        *pos += 1;
    }
    let mut value = Vec::new();
    let b = *head.get(*pos)?;
    if b == b'"' || b == b'\'' {
        *pos += 1;
        loop {
            let c = *head.get(*pos)?;
            *pos += 1;
            if c == b {
                return Some(Some((name, value)));
            }
            value.push(c.to_ascii_lowercase());
        }
    }
    if b == b'>' {
        return Some(Some((name, value)));
    }
    loop {
        let c = *head.get(*pos)?;
        if is_html_space(c) || c == b'>' {
            return Some(Some((name, value)));
        }
        value.push(c.to_ascii_lowercase());
        *pos += 1;
    }
}

/// WHATWG "extract a character encoding from a meta element" over a
/// `content` value.
fn charset_from_meta_content(content: &[u8]) -> Option<&[u8]> {
    let mut pos = 0;
    loop {
        let at = pos + find(&content[pos..], b"charset")?;
        pos = at + b"charset".len();
        while content.get(pos).is_some_and(|&b| is_html_space(b)) {
            pos += 1;
        }
        if content.get(pos) == Some(&b'=') {
            pos += 1;
            break;
        }
    }
    while content.get(pos).is_some_and(|&b| is_html_space(b)) {
        pos += 1;
    }
    match *content.get(pos)? {
        quote @ (b'"' | b'\'') => {
            let start = pos + 1;
            let end = start + content[start..].iter().position(|&b| b == quote)?;
            Some(&content[start..end])
        }
        _ => {
            let end = content[pos..]
                .iter()
                .position(|&b| is_html_space(b) || b == b';')
                .map_or(content.len(), |offset| pos + offset);
            Some(&content[pos..end])
        }
    }
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
        assert_eq!(
            charset_param("text/html; Charset=\"GBK\"").as_deref(),
            Some("GBK")
        );
        assert_eq!(
            charset_param("text/html;charset=utf-8").as_deref(),
            Some("utf-8")
        );
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
    // === R2-S8: faithful prescan ===

    #[test]
    fn a_meta_inside_another_tags_attribute_value_is_not_a_declaration() {
        let html = "<div data-example=\"<meta charset=windows-1252>\"></div>\
                    <meta charset=utf-8><p>café</p>";
        assert_eq!(decode_html(html.as_bytes(), None).unwrap(), html);
    }

    #[test]
    fn a_quoted_gt_does_not_end_the_meta_tag() {
        let mut html = b"<meta name=\"x>y\" charset=\"gbk\"><p>".to_vec();
        html.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]);
        assert_eq!(
            decode_html(&html, None).unwrap(),
            "<meta name=\"x>y\" charset=\"gbk\"><p>中文"
        );
    }

    #[test]
    fn a_declaration_unfinished_at_the_1024_byte_boundary_is_dropped() {
        // `<meta charset=gbk` fits in the first 1024 bytes, its `>` does not.
        let decl = b"<meta charset=gbk";
        let mut html = vec![b' '; PRESCAN_LIMIT - decl.len()];
        html.extend_from_slice(decl);
        html.extend_from_slice(b"><p>\xd6\xd0");
        assert!(decode_html(&html, None).is_err());
        // The same declaration completed inside the limit is honoured.
        let mut html = vec![b' '; PRESCAN_LIMIT - decl.len() - 1];
        html.extend_from_slice(decl);
        html.extend_from_slice(b"><p>\xd6\xd0");
        assert!(decode_html(&html, None).unwrap().ends_with("<p>中"));
    }

    #[test]
    fn x_user_defined_meta_means_windows_1252() {
        let html = b"<meta charset=x-user-defined><p>caf\xe9</p>";
        assert_eq!(
            decode_html(html, None).unwrap(),
            "<meta charset=x-user-defined><p>café</p>"
        );
    }

    #[test]
    fn content_type_parameters_respect_quoted_strings() {
        let html = "<p>café</p>";
        // `charset=gbk` is inside another parameter's quoted value.
        assert_eq!(
            decode_html(html.as_bytes(), Some("text/html; foo=\"bar;charset=gbk\"")).unwrap(),
            html
        );
        assert_eq!(
            charset_param("text/html; foo=\"a;b\"; charset=\"GBK\"").as_deref(),
            Some("GBK")
        );
        assert_eq!(
            charset_param("text/html; title=\"x\\\"charset=gbk\"").as_deref(),
            None
        );
    }
}
