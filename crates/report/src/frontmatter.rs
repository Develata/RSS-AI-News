/// 极简 YAML frontmatter（手写，不引 yaml crate）。
/// 字段：title / date / category / generated_at（ISO8601 UTC）。
pub fn build_frontmatter(
    title: &str,
    report_date: &str,
    category_key: &str,
    generated_at: time::OffsetDateTime,
) -> String {
    use time::format_description::well_known::Iso8601;
    let generated_iso = generated_at.format(&Iso8601::DEFAULT).unwrap_or_default();
    format!(
        "---\ntitle: {}\ndate: {}\ncategory: {}\ngenerated_at: {}\n---\n",
        yaml_escape(title),
        report_date,
        category_key,
        generated_iso,
    )
}

fn yaml_escape(value: &str) -> String {
    if value.contains([':', '#', '\n', '\'', '"']) {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_string()
    }
}
