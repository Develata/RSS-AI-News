/// 极简 YAML frontmatter（手写，不引 yaml crate）。
/// 字段：title / date / excerpt。
pub fn build_frontmatter(title: &str, report_date: &str, excerpt: &str) -> String {
    format!(
        "---\ntitle: {}\ndate: {}\nexcerpt: {}\n---\n",
        yaml_escape(title),
        report_date,
        yaml_escape(excerpt),
    )
}

/// Emits `value` as a YAML scalar that parses back as the same string — except
/// date-like values, which are kept plain by policy (see [`is_plain_safe`]).
///
/// Plain (unquoted) output is kept whenever it is unambiguous, so reports that
/// were already valid render byte-identically; values a YAML parser would
/// read as another type or reject (flow collections, aliases, booleans,
/// numbers, null, control / non-printable characters, …) are double-quoted.
pub(crate) fn yaml_escape(value: &str) -> String {
    if value.contains([':', '#', '\n', '\r', '\t', '\'', '"', '\\']) || !is_plain_safe(value) {
        let mut escaped = String::with_capacity(value.len() + 2);
        for ch in value.chars() {
            match ch {
                '\\' => escaped.push_str("\\\\"),
                '"' => escaped.push_str("\\\""),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '\t' => escaped.push_str("\\t"),
                ch if needs_unicode_escape(ch) => {
                    escaped.push_str(&format!("\\u{:04X}", ch as u32));
                }
                ch => escaped.push(ch),
            }
        }
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

/// Whether `value` is safe as a YAML plain scalar that resolves to a string
/// under YAML 1.1 and the 1.2 core schema. `: # ' " \\ \n \r \t` are
/// handled by the caller.
///
/// Date-like values (`2026-05-18`) are intentionally left plain: the report
/// date has always been emitted that way (e.g. `{title_yaml}`), and site
/// generators rely on reading it as a date; quoting it would change every
/// historical report.
fn is_plain_safe(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false; // empty plain scalar is null
    };
    if value.trim() != value || value.chars().any(needs_unicode_escape) {
        return false;
    }
    // Indicators that can never start a plain scalar.
    if "[]{},&*!|>%@`".contains(first) {
        return false;
    }
    // `-`, `?`, `:` start a plain scalar only when followed by a non-space.
    if "-?:".contains(first) && chars.next().is_none_or(char::is_whitespace) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "~" | "null" | "true" | "false" | "yes" | "no" | "on" | "off" | "y" | "n"
    ) {
        return false;
    }
    !is_yaml_number(&lower)
}

/// Characters that must not appear raw in a YAML scalar: C0/C1 controls and
/// DEL (non-printable), U+FFFE / U+FFFF (outside YAML's character set) and
/// the YAML 1.1 line separators U+2028 / U+2029.
fn needs_unicode_escape(ch: char) -> bool {
    ch.is_control() || matches!(ch, '\u{FFFE}' | '\u{FFFF}' | '\u{2028}' | '\u{2029}')
}

/// YAML 1.1 / 1.2 ints and floats, including `_` separators, `0b` / `0o` /
/// `0x` prefixes and `.inf` / `.nan`. Follows the YAML lexical form: at most
/// one sign, then a digit or `.digit`; `_` only after the first digit (so
/// `_1` or `+_1` stay strings).
fn is_yaml_number(lower: &str) -> bool {
    let unsigned = lower.strip_prefix(['+', '-']).unwrap_or(lower);
    if matches!(unsigned, ".inf" | ".nan") {
        return true;
    }
    let starts_numeric = unsigned.starts_with(|ch: char| ch.is_ascii_digit())
        || (unsigned.starts_with('.') && unsigned[1..].starts_with(|ch: char| ch.is_ascii_digit()));
    if !starts_numeric {
        return false;
    }
    let digits = unsigned.replace('_', "");
    for (prefix, radix) in [("0b", 2), ("0o", 8), ("0x", 16)] {
        if let Some(rest) = digits.strip_prefix(prefix) {
            return !rest.is_empty() && rest.chars().all(|ch| ch.is_digit(radix));
        }
    }
    // The leading digit check above also keeps Rust-only float spellings
    // ("inf", "nan") out.
    digits.parse::<f64>().is_ok()
}
