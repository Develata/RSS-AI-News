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

/// Emits `value` as a YAML scalar that parses back as the same string.
///
/// Plain (unquoted) output is kept whenever it is unambiguous, so reports that
/// were already valid render byte-identically; values a YAML parser would
/// read as another type or reject (flow collections, aliases, booleans,
/// numbers, null, …) are double-quoted.
pub(crate) fn yaml_escape(value: &str) -> String {
    if value.contains([':', '#', '\n', '\r', '\t', '\'', '"', '\\']) || !is_plain_safe(value) {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

/// Whether `value` is safe as a YAML plain scalar that resolves to a string
/// (YAML 1.1 and 1.2 core schemas). Characters `: # ' " \\` and control
/// characters are handled by the caller.
fn is_plain_safe(value: &str) -> bool {
    let Some(first) = value.chars().next() else {
        return false; // empty plain scalar is null
    };
    if value.trim() != value {
        return false;
    }
    // Indicator characters may not start a plain scalar.
    if "-?,[]{}&*!|>%@`".contains(first) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "~" | "null" | "true" | "false" | "yes" | "no" | "on" | "off" | "y" | "n"
    ) {
        return false;
    }
    // Numbers (int, float, hex/octal, .inf/.nan) would resolve to non-strings.
    let numeric = lower.trim_start_matches(['+', '-']);
    !(value.parse::<f64>().is_ok()
        || numeric.starts_with("0x")
        || numeric.starts_with("0o")
        || matches!(numeric, ".inf" | ".nan"))
}
