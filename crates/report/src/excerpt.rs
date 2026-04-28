/// 字符级（非字节级）安全裁剪到 `max_chars`，溢出时追加 "…"。
pub fn generate_excerpt(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}
