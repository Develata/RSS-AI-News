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

    template
        .replace("{title}", input.title)
        .replace("{body_text}", &body_text)
        .replace("{category_key}", input.category_key)
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let mut output = chars.by_ref().take(max_chars).collect::<String>();

    if chars.next().is_some() {
        output.push('…');
    }

    output
}
