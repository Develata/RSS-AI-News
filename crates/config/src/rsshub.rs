const RSSHUB_BASE_PLACEHOLDERS: [&str; 2] = ["{RSSHUB}", "{RSSHUB_BASE_URL}"];

pub(crate) fn has_base_placeholder(value: &str) -> bool {
    RSSHUB_BASE_PLACEHOLDERS
        .iter()
        .any(|placeholder| value.contains(placeholder))
}

pub(crate) fn expand_base_placeholders(value: &str, base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    RSSHUB_BASE_PLACEHOLDERS
        .iter()
        .fold(value.to_string(), |expanded, placeholder| {
            expanded.replace(placeholder, base_url)
        })
}
