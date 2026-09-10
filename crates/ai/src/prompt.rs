use std::borrow::Cow;

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

fn truncate_chars(input: &str, max_chars: usize) -> Cow<'_, str> {
    match input.char_indices().nth(max_chars) {
        Some((end, _)) => {
            let mut output = String::with_capacity(end + '…'.len_utf8());
            output.push_str(&input[..end]);
            output.push('…');
            Cow::Owned(output)
        }
        None => Cow::Borrowed(input),
    }
}
