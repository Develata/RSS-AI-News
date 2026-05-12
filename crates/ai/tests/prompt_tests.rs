use rss_ai_news_ai::{PromptInput, PromptRenderConfig, render_prompt};

#[test]
fn render_substitutes_title_body_category_placeholders() {
    let rendered = render_prompt(
        "标题={title}; 分类={category_key}; 正文={body_text}; 未知={unknown}",
        &PromptInput {
            title: "T",
            body_text: "B",
            category_key: "C",
        },
        &PromptRenderConfig {
            max_input_chars: 10,
        },
    );

    assert_eq!(rendered, "标题=T; 分类=C; 正文=B; 未知={unknown}");
}

#[test]
fn render_truncates_body_to_max_chars_with_ellipsis() {
    let rendered = render_prompt(
        "{body_text}",
        &PromptInput {
            title: "",
            body_text: "abcdef",
            category_key: "",
        },
        &PromptRenderConfig { max_input_chars: 3 },
    );

    assert_eq!(rendered, "abc…");
}

#[test]
fn render_does_not_break_utf8_at_boundary() {
    let rendered = render_prompt(
        "{body_text}",
        &PromptInput {
            title: "",
            body_text: "中a文b混c排",
            category_key: "",
        },
        &PromptRenderConfig { max_input_chars: 5 },
    );

    assert_eq!(rendered, "中a文b混…");
}

#[test]
fn render_does_not_re_substitute_placeholders_inside_replaced_content() {
    // 若 title 自身含 `{body_text}` 字面文本，单次遍历不应把它当作占位符二次替换。
    let rendered = render_prompt(
        "T={title}; B={body_text}",
        &PromptInput {
            title: "{body_text}",
            body_text: "BODY",
            category_key: "",
        },
        &PromptRenderConfig {
            max_input_chars: 100,
        },
    );

    assert_eq!(rendered, "T={body_text}; B=BODY");
}
