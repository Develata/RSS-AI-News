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

pub(crate) fn yaml_escape(value: &str) -> String {
    if value.contains([':', '#', '\n', '\r', '\t', '\'', '"', '\\']) {
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
