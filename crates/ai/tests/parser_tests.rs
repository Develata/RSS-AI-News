use rss_ai_news_ai::{AiError, ParsedResponse, parse_response};

#[test]
fn parse_keep_true_returns_ai_output() {
    let raw = r#"{
        "summary": "摘要",
        "tags": ["a", "b"],
        "importance_score": 80,
        "keep_decision": true,
        "filter_reason": null,
        "extra": "ignored"
    }"#;

    let parsed = parse_response(7, raw).expect("valid output");
    let ParsedResponse::Output(output) = parsed else {
        panic!("expected output");
    };

    assert_eq!(output.article_ai_result_id, 7);
    assert_eq!(output.summary, "摘要");
    assert_eq!(output.tags, vec!["a", "b"]);
    assert_eq!(output.importance_score.get(), 80);
    assert!(output.keep_decision);
    assert_eq!(output.raw_response, raw);
}

#[test]
fn parse_keep_false_returns_filtered_output() {
    let raw = r#"{
        "keep_decision": false,
        "filter_reason": "重复内容"
    }"#;

    let parsed = parse_response(7, raw).expect("valid filtered output");
    let ParsedResponse::Filtered(output) = parsed else {
        panic!("expected filtered output");
    };

    assert_eq!(output.article_ai_result_id, 7);
    assert_eq!(output.reason, "重复内容");
    assert_eq!(output.raw_response, raw);
}

#[test]
fn parse_extracts_json_from_text_with_prefix_and_suffix() {
    let raw = r#"好的，结果如下：{"summary":"含 { 字符串 }","tags":["x"],"importance_score":90,"keep_decision":true}
谢谢"#;

    let parsed = parse_response(9, raw).expect("json object should be extracted");
    let ParsedResponse::Output(output) = parsed else {
        panic!("expected output");
    };

    assert_eq!(output.article_ai_result_id, 9);
    assert_eq!(output.summary, "含 { 字符串 }");
    assert_eq!(output.importance_score.get(), 90);
}

#[test]
fn parse_returns_missing_field_when_keep_true_without_summary() {
    let raw = r#"{
        "tags": [],
        "importance_score": 80,
        "keep_decision": true
    }"#;

    let err = parse_response(7, raw).expect_err("summary is required");
    assert!(matches!(err, AiError::MissingField { field } if field == "summary"));
}

#[test]
fn parse_returns_invalid_field_value_when_score_out_of_range() {
    let raw = r#"{
        "summary": "摘要",
        "tags": [],
        "importance_score": 101,
        "keep_decision": true
    }"#;

    let err = parse_response(7, raw).expect_err("score range is enforced");
    assert!(matches!(err, AiError::InvalidFieldValue { field, .. } if field == "importance_score"));
}

#[test]
fn parse_returns_invalid_json_for_garbage_input() {
    let err = parse_response(7, "not json").expect_err("garbage is invalid");
    assert!(matches!(err, AiError::InvalidJson(_)));
}
