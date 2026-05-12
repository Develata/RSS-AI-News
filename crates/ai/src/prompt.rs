pub struct PromptInput<'a> {
    pub title: &'a str,
    pub body_text: &'a str,
    pub category_key: &'a str,
}

pub struct PromptRenderConfig {
    pub max_input_chars: usize,
}

pub fn render_prompt(template: &str, input: &PromptInput<'_>, cfg: &PromptRenderConfig) -> String {
    let body_text = truncate_chars(input.body_text, cfg.max_input_chars);
    let mut output = String::with_capacity(template.len() + body_text.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some(stripped) = after.strip_prefix("{title}") {
            output.push_str(input.title);
            rest = stripped;
        } else if let Some(stripped) = after.strip_prefix("{body_text}") {
            output.push_str(&body_text);
            rest = stripped;
        } else if let Some(stripped) = after.strip_prefix("{category_key}") {
            output.push_str(input.category_key);
            rest = stripped;
        } else {
            output.push('{');
            rest = &after[1..];
        }
    }
    output.push_str(rest);
    output
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let mut output = chars.by_ref().take(max_chars).collect::<String>();

    if chars.next().is_some() {
        output.push('…');
    }

    output
}
