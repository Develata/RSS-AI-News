use std::sync::LazyLock;

use regex::Regex;

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
    !is_yaml_reserved(value) && !is_yaml_number(value)
}

/// Characters that must not appear raw in a YAML scalar: C0/C1 controls and
/// DEL (non-printable), U+FFFE / U+FFFF (outside YAML's character set), the
/// byte order mark U+FEFF (excluded from plain content) and the YAML 1.1 line
/// separators U+2028 / U+2029.
fn needs_unicode_escape(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{FEFF}' | '\u{FFFE}' | '\u{FFFF}' | '\u{2028}' | '\u{2029}'
        )
}

/// Exact spellings that PyYAML (YAML 1.1) or the YAML 1.2 core schema
/// resolve to bool, null, merge (`<<`) or value (`=`). Casing is exact: the
/// resolvers do not accept e.g. `tRuE` or `y`, which therefore stay plain.
fn is_yaml_reserved(value: &str) -> bool {
    matches!(
        value,
        "~" | "null"
            | "Null"
            | "NULL"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | "yes"
            | "Yes"
            | "YES"
            | "no"
            | "No"
            | "NO"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
            | "<<"
            | "="
    )
}

/// YAML numbers under the union of the two resolvers readers use:
/// PyYAML's YAML 1.1 implicit int/float patterns (`_` separators, `0b`,
/// leading-0 octal, sexagesimal, `.inf`) and the YAML 1.2 core schema
/// (`0o`, exponent without sign). Matching the reference patterns on the raw
/// value keeps valid strings such as `_1`, `1e_3` or `._1` plain, and quotes
/// forms like `0b_` that PyYAML resolves as int but cannot construct.
fn is_yaml_number(value: &str) -> bool {
    static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            "^(?:",
            // PyYAML (YAML 1.1) int
            r"[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+",
            r"|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+",
            // PyYAML (YAML 1.1) float
            r"|[-+]?[0-9][0-9_]*\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
            r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*",
            r"|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
            // YAML 1.2 core schema int / float
            r"|[-+]?[0-9]+|0o[0-7]+|0x[0-9a-fA-F]+",
            r"|[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)(?:[eE][-+]?[0-9]+)?",
            ")$"
        ))
        .expect("static YAML number pattern")
    });
    NUMBER.is_match(value)
}
