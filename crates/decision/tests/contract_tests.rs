//! Offline contract tests against wiremock and a raw loopback socket
//! (ADR 0010 §3–§5): exact request shapes per provider, normalisation of
//! well-formed responses, the response rejection rules, and every error class.
//! No real provider is contacted. Clients are built with `use_env_proxy:
//! false`, so a developer `HTTP_PROXY` never intercepts the loopback servers.

use std::time::Duration;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpSocket},
};

use rss_ai_news_decision::{
    ChoiceOption, ClientSettings, DecisionAnswer, DecisionClient, DecisionError, DecisionRequest,
    Exchange, Provider, QuestionKind, QuestionSpec, SpreadKind, StateTruncation, SystemOneClient,
    Withheld, canonical_policy, revalidate,
};
use rss_ai_news_domain::SecretString;
use serde_json::{Value, json};
use url::Url;

/// Far enough that only the per-request timeout applies.
fn deadline() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(3600)
}
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const KEY: &str = "sk-test-key-123";

fn questions() -> Vec<QuestionSpec> {
    vec![
        QuestionSpec {
            id: "q_marketing".to_string(),
            instructions: "这篇是否主要是营销稿？".to_string(),
            kind: QuestionKind::Noul {
                yes: Some("营销".to_string()),
                no: Some("报道".to_string()),
            },
        },
        QuestionSpec {
            id: "q_kind".to_string(),
            instructions: "哪一类？".to_string(),
            kind: QuestionKind::Choice {
                options: vec![
                    ChoiceOption {
                        label: "paper".to_string(),
                        description: Some("论文".to_string()),
                    },
                    ChoiceOption {
                        label: "other".to_string(),
                        description: None,
                    },
                ],
            },
        },
        QuestionSpec {
            id: "q_importance".to_string(),
            instructions: "重要性？".to_string(),
            kind: QuestionKind::Score {
                levels: vec!["琐碎".to_string(), "一般".to_string(), "重大".to_string()],
            },
        },
    ]
}

fn client(provider: Provider, base: &str, model: &str, timeout: Duration) -> SystemOneClient {
    SystemOneClient::new(
        ClientSettings {
            provider,
            base_url: Url::parse(base).unwrap(),
            model: model.to_string(),
            request_timeout: timeout,
            max_state_chars: 6000,
            laya_max_len: 2048,
            use_env_proxy: false,
        },
        SecretString::from(KEY),
    )
    .unwrap()
}

fn laya_ok() -> Value {
    json!({
        "model": "laya-rl-agent",
        "answers": {
            "q_marketing": {"type": "noul", "noul": 0.8123, "confidence": 0.8123,
                            "answer_confidence": 0.8123, "action": {"act_probability": 0.5}},
            "q_kind": {"type": "choice", "choice": "paper",
                       "probabilities": {"paper": 0.7, "other": 0.3},
                       "confidence": 0.1187, "answer_confidence": 0.7, "action": {}},
            "q_importance": {"type": "score", "score": 1.4,
                             "legend": {"0": "琐碎", "1": "一般", "2": "重大"},
                             "probabilities": {"0": 0.1, "1": 0.4, "2": 0.5},
                             "confidence": 0.1, "answer_confidence": 0.5, "action": {}}
        },
        "usage": {"input_tokens": 321, "output_tokens": 0, "state_tokens": 300,
                  "state_tokens_dropped": 0, "truncated": false, "truncated_questions": []},
        "routing": {"model": "multilingual", "lang": "zh"}
    })
}

/// Shaped like the docs.typesafe.ai examples, including their two-decimal
/// precision (score confidence 0.35 where the formula gives 0.355).
fn jev_ok() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "q_marketing": {"type": "noul", "noul": 0.82},
            "q_kind": {"type": "choice", "choice": "paper",
                       "probabilities": {"paper": 0.7, "other": 0.3}, "confidence": 0.4},
            "q_importance": {"type": "score", "score": 1.43,
                             "legend": {"0": "琐碎", "1": "一般", "2": "重大"},
                             "probabilities": {"0": 0.0, "1": 0.57, "2": 0.43},
                             "confidence": 0.35}
        },
        "usage": {"input_tokens": 512, "output_tokens": 0}
    })
}

async fn serve(template: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(template)
        .mount(&server)
        .await;
    server
}

async fn ask(
    provider: Provider,
    model: &str,
    template: ResponseTemplate,
) -> (
    MockServer,
    Result<rss_ai_news_decision::DecisionResponse, DecisionError>,
) {
    let server = serve(template).await;
    let qs = questions();
    let result = client(provider, &server.uri(), model, Duration::from_secs(10))
        .decide(
            &DecisionRequest {
                title: "标题",
                body: "正文",
                questions: &qs,
            },
            deadline(),
        )
        .await
        .result;
    (server, result)
}

async fn sent_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        &format!("Bearer {KEY}")
    );
    serde_json::from_slice(&requests[0].body).unwrap()
}

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[tokio::test]
async fn laya_request_has_exactly_four_fields_and_an_explicit_max_len() {
    let (server, result) = ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(200).set_body_json(laya_ok()),
    )
    .await;
    result.unwrap();
    let body = sent_body(&server).await;
    assert_eq!(keys(&body), ["max_len", "model", "questions", "state"]);
    assert_eq!(body["max_len"], 2048);
    assert_eq!(body["model"], "multilingual");
    assert_eq!(body["state"], json!({"title": "标题", "body": "正文"}));
    // laya-server encodes an empty description as the bare label.
    assert_eq!(body["questions"], expected_questions(json!("")));
}

/// The full `questions` object both providers receive for `questions()`;
/// only the absent choice description differs.
fn expected_questions(absent_description: Value) -> Value {
    json!({
        "q_marketing": {"type": "noul", "instructions": "这篇是否主要是营销稿？",
                        "criteria": {"true": "营销", "false": "报道"}},
        "q_kind": {"type": "choice", "instructions": "哪一类？",
                   "criteria": {"paper": "论文", "other": absent_description}},
        "q_importance": {"type": "score", "instructions": "重要性？",
                         "criteria": ["琐碎", "一般", "重大"]}
    })
}

#[tokio::test]
async fn jev_request_has_no_max_len_and_null_descriptions() {
    let (server, result) = ask(
        Provider::Jev,
        "jev-1.13.0",
        ResponseTemplate::new(200).set_body_json(jev_ok()),
    )
    .await;
    result.unwrap();
    let body = sent_body(&server).await;
    assert_eq!(keys(&body), ["model", "questions", "state"]);
    assert_eq!(body["model"], "jev-1.13.0");
    assert_eq!(body["state"], json!({"title": "标题", "body": "正文"}));
    assert_eq!(body["questions"], expected_questions(Value::Null));
}

#[tokio::test]
async fn laya_response_is_normalised_with_provenance() {
    let (_server, result) = ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(200).set_body_json(laya_ok()),
    )
    .await;
    let response = result.unwrap();
    assert_eq!(response.execution.reported_model, "laya-rl-agent");
    assert_eq!(
        response.execution.routing_model.as_deref(),
        Some("multilingual")
    );
    assert_eq!(
        response.usage.state,
        StateTruncation::Laya {
            truncated: false,
            state_tokens_dropped: 0
        }
    );
    let ids: Vec<_> = response.answers.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["q_marketing", "q_kind", "q_importance"]);
    assert_eq!(
        response.answers[0].answer,
        DecisionAnswer::Noul {
            p_yes: 0.8123,
            top_probability: Some(0.8123)
        }
    );
    let DecisionAnswer::Choice {
        label,
        probabilities,
        spread_confidence,
        top_probability,
    } = &response.answers[1].answer
    else {
        panic!("choice");
    };
    assert_eq!(label, "paper");
    assert_eq!(probabilities, &[0.7, 0.3]);
    assert_eq!(spread_confidence.kind, SpreadKind::LayaEntropy);
    assert_eq!(*top_probability, Some(0.7));
    let DecisionAnswer::Score {
        expected,
        spread_confidence,
        ..
    } = &response.answers[2].answer
    else {
        panic!("score");
    };
    assert_eq!(*expected, 1.4);
    assert_eq!(spread_confidence.kind, SpreadKind::LayaEntropy);
}

#[tokio::test]
async fn jev_documented_examples_pass_with_two_decimal_precision() {
    let (_server, result) = ask(
        Provider::Jev,
        "jev-1.13.0",
        ResponseTemplate::new(200).set_body_json(jev_ok()),
    )
    .await;
    let response = result.unwrap();
    assert_eq!(response.execution.routing_model, None);
    assert_eq!(response.usage.state, StateTruncation::NotApplicable);
    assert_eq!(
        response.answers[0].answer,
        DecisionAnswer::Noul {
            p_yes: 0.82,
            top_probability: None
        }
    );
    let DecisionAnswer::Score {
        spread_confidence, ..
    } = &response.answers[2].answer
    else {
        panic!("score");
    };
    assert_eq!(spread_confidence.kind, SpreadKind::JevScore);
    assert_eq!(spread_confidence.value, 0.35);
}

#[tokio::test]
async fn jev_alias_accepts_any_reported_version_but_a_pin_must_match() {
    let (_server, result) = ask(
        Provider::Jev,
        "jev-latest",
        ResponseTemplate::new(200).set_body_json(jev_ok()),
    )
    .await;
    result.unwrap();
    let mut other = jev_ok();
    other["model"] = json!("jev-1.14.0");
    let (_server, result) = ask(
        Provider::Jev,
        "jev-1.13.0",
        ResponseTemplate::new(200).set_body_json(other),
    )
    .await;
    assert_invalid(result, "reported model");
}

#[tokio::test]
async fn state_body_is_cut_to_max_state_chars_on_a_char_boundary() {
    let server = serve(ResponseTemplate::new(200).set_body_json(laya_ok())).await;
    let mut settings = ClientSettings {
        provider: Provider::Laya,
        base_url: Url::parse(&server.uri()).unwrap(),
        model: "multilingual".to_string(),
        request_timeout: Duration::from_secs(10),
        max_state_chars: 3,
        laya_max_len: 2048,
        use_env_proxy: false,
    };
    let qs = questions();
    SystemOneClient::new(settings.clone(), SecretString::from(KEY))
        .unwrap()
        .decide(
            &DecisionRequest {
                title: "t",
                body: "中文正文内容",
                questions: &qs,
            },
            deadline(),
        )
        .await
        .result
        .unwrap();
    assert_eq!(sent_body(&server).await["state"]["body"], "中文正");
    settings.max_state_chars = 0;
    assert!(SystemOneClient::new(settings, SecretString::from(KEY)).is_err());
}

fn assert_invalid(
    result: Result<rss_ai_news_decision::DecisionResponse, DecisionError>,
    reason: &str,
) {
    match result {
        Err(DecisionError::InvalidResponse { reason: actual }) => {
            assert!(actual.contains(reason), "{actual:?} lacks {reason:?}");
        }
        other => panic!("expected InvalidResponse({reason}), got {other:?}"),
    }
}

async fn laya_with(
    mutate: impl FnOnce(&mut Value),
) -> Result<rss_ai_news_decision::DecisionResponse, DecisionError> {
    let mut body = laya_ok();
    mutate(&mut body);
    ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(200).set_body_json(body),
    )
    .await
    .1
}

#[tokio::test]
async fn laya_rejects_missing_or_mismatched_metadata() {
    assert_invalid(
        laya_with(|b| {
            b.as_object_mut().unwrap().remove("routing");
        })
        .await,
        "routing.model is missing",
    );
    assert_invalid(
        laya_with(|b| b["routing"]["model"] = json!("english")).await,
        "routing.model does not match",
    );
    assert_invalid(
        laya_with(|b| {
            b["usage"].as_object_mut().unwrap().remove("truncated");
        })
        .await,
        "usage.truncated is missing",
    );
    assert_invalid(
        laya_with(|b| {
            b["usage"]["options"] =
                json!({"q_kind": {"total": 2, "distinct": 1, "tokens_per_option": 3}});
        })
        .await,
        "options collapsed",
    );
}

#[tokio::test]
async fn laya_rejects_answers_inconsistent_with_their_distribution() {
    assert_invalid(
        laya_with(|b| b["answers"]["q_kind"]["choice"] = json!("other")).await,
        "most probable",
    );
    assert_invalid(
        laya_with(|b| b["answers"]["q_importance"]["score"] = json!(0.2)).await,
        "does not match its level probabilities",
    );
    for (id, wrong) in [("q_marketing", 0.5), ("q_kind", 0.9), ("q_importance", 0.9)] {
        assert_invalid(
            laya_with(|b| b["answers"][id]["answer_confidence"] = json!(wrong)).await,
            &format!("question {id}: answer_confidence does not match"),
        );
    }
    assert_invalid(
        laya_with(|b| b["answers"]["q_marketing"]["confidence"] = json!(0.5)).await,
        "question q_marketing: confidence does not match",
    );
    assert_invalid(
        laya_with(|b| b["answers"]["q_kind"]["probabilities"]["other"] = json!(0.2)).await,
        "sum to 1",
    );
    assert_invalid(
        laya_with(|b| b["answers"]["q_marketing"]["noul"] = json!(1.5)).await,
        "[0, 1]",
    );
}

const LAYA_CONFIDENCES: [(&str, &str); 6] = [
    ("q_marketing", "confidence"),
    ("q_marketing", "answer_confidence"),
    ("q_kind", "confidence"),
    ("q_kind", "answer_confidence"),
    ("q_importance", "confidence"),
    ("q_importance", "answer_confidence"),
];

#[tokio::test]
async fn laya_requires_both_confidences_on_every_answer_type() {
    for (id, field) in LAYA_CONFIDENCES {
        assert_invalid(
            laya_with(|b| {
                b["answers"][id].as_object_mut().unwrap().remove(field);
            })
            .await,
            &format!("question {id}: {field} is missing"),
        );
    }
}

#[tokio::test]
async fn confidences_outside_the_unit_interval_are_rejected() {
    for (id, field) in LAYA_CONFIDENCES {
        for wrong in [1.5, -0.1] {
            assert_invalid(
                laya_with(|b| b["answers"][id][field] = json!(wrong)).await,
                &format!("question {id}: {field} must be a finite number in [0, 1]"),
            );
        }
    }
    // Jev documents no noul confidence; one that is present must still be sane.
    for (id, wrong) in [
        ("q_marketing", 2.0),
        ("q_kind", -0.1),
        ("q_importance", 1.5),
    ] {
        let mut body = jev_ok();
        body["answers"][id]["confidence"] = json!(wrong);
        let (_server, result) = ask(
            Provider::Jev,
            "jev-1.13.0",
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await;
        assert_invalid(
            result,
            &format!("question {id}: confidence must be a finite number in [0, 1]"),
        );
    }
}

#[tokio::test]
async fn laya_rejects_missing_extra_and_mistyped_answers() {
    assert_invalid(
        laya_with(|b| {
            b["answers"].as_object_mut().unwrap().remove("q_importance");
        })
        .await,
        "no answer for question q_importance",
    );
    assert_invalid(
        laya_with(|b| b["answers"]["q_extra"] = b["answers"]["q_marketing"].clone()).await,
        "unknown questions",
    );
    assert_invalid(
        laya_with(|b| b["answers"]["q_marketing"] = b["answers"]["q_kind"].clone()).await,
        "type does not match",
    );
}

#[tokio::test]
async fn duplicate_json_keys_are_rejected_not_overwritten() {
    let text = laya_ok().to_string().replacen(
        "\"q_marketing\":",
        "\"q_marketing\":{\"type\":\"noul\",\"noul\":0.1,\"confidence\":0.9,\"answer_confidence\":0.9},\"q_marketing\":",
        1,
    );
    let (_server, result) = ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(200).set_body_raw(text, "application/json"),
    )
    .await;
    assert_invalid(result, "decision schema");
}

#[tokio::test]
async fn jev_requires_choice_and_score_confidence() {
    for id in ["q_kind", "q_importance"] {
        let mut body = jev_ok();
        body["answers"][id]
            .as_object_mut()
            .unwrap()
            .remove("confidence");
        let (_server, result) = ask(
            Provider::Jev,
            "jev-1.13.0",
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await;
        assert_invalid(result, &format!("question {id}: confidence is missing"));
    }
}

async fn laya_raw(
    mutate: impl FnOnce(String) -> String,
) -> Result<rss_ai_news_decision::DecisionResponse, DecisionError> {
    let text = mutate(laya_ok().to_string());
    ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(200).set_body_raw(text, "application/json"),
    )
    .await
    .1
}

#[tokio::test]
async fn wire_numbers_outside_the_contract_are_rejected() {
    for tokens in ["-1", "1e3", "321.0", "18446744073709551616", "\"321\""] {
        let result = laya_raw(|text| {
            text.replacen(
                "\"input_tokens\":321",
                &format!("\"input_tokens\":{tokens}"),
                1,
            )
        })
        .await;
        assert_invalid(result, "decision schema");
    }
    // NaN is not JSON; serde_json refuses it before any range check.
    let result = laya_raw(|text| text.replacen("\"noul\":0.8123", "\"noul\":NaN", 1)).await;
    assert_invalid(result, "decision schema");
    let result = laya_raw(|text| text.replacen("\"noul\":0.8123", "\"noul\":\"0.8123\"", 1)).await;
    assert_invalid(result, "decision schema");
    // Exponent notation is an ordinary JSON number for probabilities.
    laya_raw(|text| {
        assert!(text.contains("\"paper\":0.7"));
        text.replacen("\"paper\":0.7", "\"paper\":7e-1", 1)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn laya_requires_every_provenance_field() {
    assert_invalid(
        laya_with(|b| {
            b["usage"]
                .as_object_mut()
                .unwrap()
                .remove("state_tokens_dropped");
        })
        .await,
        "usage.state_tokens_dropped is missing",
    );
    assert_invalid(
        laya_with(|b| b["routing"]["model"] = Value::Null).await,
        "routing.model is missing",
    );
    assert_invalid(
        laya_with(|b| {
            b.as_object_mut().unwrap().remove("model");
        })
        .await,
        "decision schema",
    );
}

async fn status(code: u16, body: Value) -> DecisionError {
    ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(code).set_body_json(body),
    )
    .await
    .1
    .unwrap_err()
}

#[tokio::test]
async fn every_status_maps_to_a_closed_error_class() {
    assert_eq!(
        status(401, json!({"detail": {"code": "INVALID_API_KEY"}})).await,
        DecisionError::Unauthorized { status: 401 }
    );
    assert_eq!(
        status(
            422,
            json!({"detail": {"code": "VALIDATION_ERROR", "errors": []}})
        )
        .await,
        DecisionError::Rejected {
            status: 422,
            code: Some("VALIDATION_ERROR".to_string())
        }
    );
    assert_eq!(
        status(400, json!({"error": "max_tokens_exceeded"})).await,
        DecisionError::Rejected {
            status: 400,
            code: Some("max_tokens_exceeded".to_string())
        }
    );
    assert_eq!(status(413, json!({})).await, DecisionError::PayloadTooLarge);
    assert_eq!(
        status(415, json!({})).await,
        DecisionError::UnsupportedEncoding
    );
    assert_eq!(
        status(503, json!({"detail": {"code": "MODEL_UNAVAILABLE"}})).await,
        DecisionError::Unavailable {
            status: 503,
            code: Some("MODEL_UNAVAILABLE".to_string())
        }
    );
    assert_eq!(
        status(529, json!({})).await,
        DecisionError::Unavailable {
            status: 529,
            code: None
        }
    );
    assert_eq!(
        status(500, json!({})).await,
        DecisionError::Server { status: 500 }
    );
    assert_eq!(
        status(418, json!({})).await,
        DecisionError::HttpStatus { status: 418 }
    );
    assert_eq!(status(408, json!({})).await, DecisionError::RequestTimeout);
    for (header, seconds) in [
        ("7", Some(7)),
        ("+7", None),
        ("Wed, 21 Oct 2026 07:28:00 GMT", None),
    ] {
        let (_server, result) = ask(
            Provider::Laya,
            "multilingual",
            ResponseTemplate::new(429).insert_header("retry-after", header),
        )
        .await;
        assert_eq!(
            result.unwrap_err(),
            DecisionError::RateLimited {
                retry_after_seconds: seconds
            },
            "{header:?}"
        );
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    for code in [302, 307, 308] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(
                ResponseTemplate::new(code)
                    .insert_header("location", format!("{}/elsewhere", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/elsewhere"))
            .respond_with(ResponseTemplate::new(200).set_body_json(laya_ok()))
            .expect(0)
            .mount(&server)
            .await;
        let qs = questions();
        let error = client(
            Provider::Laya,
            &server.uri(),
            "multilingual",
            Duration::from_secs(5),
        )
        .decide(
            &DecisionRequest {
                title: "t",
                body: "b",
                questions: &qs,
            },
            deadline(),
        )
        .await
        .result
        .unwrap_err();
        assert_eq!(error, DecisionError::HttpStatus { status: code });
        server.verify().await;
    }
}

#[tokio::test]
async fn padded_specs_and_one_sided_noul_criteria_are_sent_normalised() {
    for (provider, model, ok, absent) in [
        (Provider::Laya, "multilingual", laya_ok(), json!("")),
        (Provider::Jev, "jev-1.13.0", jev_ok(), Value::Null),
    ] {
        let server = serve(ResponseTemplate::new(200).set_body_json(ok)).await;
        let mut qs = questions();
        qs[0].id = " q_marketing ".to_string();
        qs[0].instructions = " 这篇是否主要是营销稿？\n".to_string();
        qs[0].kind = QuestionKind::Noul {
            yes: Some(" 营销 ".to_string()),
            no: Some("  ".to_string()),
        };
        if let QuestionKind::Choice { options } = &mut qs[1].kind {
            options[0].label = "paper ".to_string();
        }
        let response = client(provider, &server.uri(), model, Duration::from_secs(5))
            .decide(
                &DecisionRequest {
                    title: "t",
                    body: "b",
                    questions: &qs,
                },
                deadline(),
            )
            .await
            .result
            .unwrap();
        assert_eq!(response.answers[0].id, "q_marketing");
        let mut expected = expected_questions(absent);
        expected["q_marketing"]["criteria"] = json!({"true": "营销"});
        assert_eq!(
            sent_body(&server).await["questions"],
            expected,
            "{provider:?}"
        );
    }
}

/// A loopback server that sends `head` and then either holds the connection
/// open (`hold`) or closes it. It reads the request first, so the client never
/// sees a reset while still writing.
async fn raw_server(head: &'static str, hold: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                // The client gave up; the test's own assertion reports why.
                return;
            }
            request.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket.write_all(head.as_bytes()).await.unwrap();
        if hold {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    format!("http://{address}")
}

async fn ask_raw(base: &str) -> DecisionError {
    let qs = questions();
    client(Provider::Laya, base, "multilingual", Duration::from_secs(1))
        .decide(
            &DecisionRequest {
                title: "t",
                body: "b",
                questions: &qs,
            },
            deadline(),
        )
        .await
        .result
        .unwrap_err()
}

#[tokio::test]
async fn an_error_body_that_stalls_or_breaks_off_keeps_its_status() {
    const STALLED_503: &str =
        "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 1000\r\n\r\n{\"detail\":";
    let unavailable = DecisionError::Unavailable {
        status: 503,
        code: None,
    };
    assert_eq!(
        ask_raw(&raw_server(STALLED_503, true).await).await,
        unavailable
    );
    assert_eq!(
        ask_raw(&raw_server(STALLED_503, false).await).await,
        unavailable
    );
    const STALLED_200: &str = "HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n{\"model\":";
    assert_eq!(
        ask_raw(&raw_server(STALLED_200, true).await).await,
        DecisionError::Timeout { seconds: 1 }
    );
}

#[tokio::test]
async fn error_bodies_are_never_echoed() {
    let error = status(422, json!({"detail": {"code": KEY, "message": KEY}})).await;
    assert_eq!(
        error,
        DecisionError::Rejected {
            status: 422,
            code: None
        }
    );
    let (_server, result) = ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(200).set_body_raw(format!("not json {KEY}"), "text/plain"),
    )
    .await;
    let error = result.unwrap_err();
    assert!(matches!(error, DecisionError::InvalidResponse { .. }));
    assert!(!error.to_string().contains(KEY));
}

#[tokio::test]
async fn oversized_bodies_are_capped_and_error_status_is_kept() {
    let huge = "x".repeat(rss_ai_news_decision::client::MAX_RESPONSE_BODY_BYTES + 1);
    let (_server, result) = ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(200).set_body_raw(huge.clone(), "application/json"),
    )
    .await;
    assert!(matches!(
        result.unwrap_err(),
        DecisionError::ResponseTooLarge { .. }
    ));
    let (_server, result) = ask(
        Provider::Laya,
        "multilingual",
        ResponseTemplate::new(503).set_body_raw(huge, "text/html"),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        DecisionError::Unavailable {
            status: 503,
            code: None
        }
    );
}

#[tokio::test]
async fn slow_responses_time_out() {
    let server = serve(
        ResponseTemplate::new(200)
            .set_body_json(laya_ok())
            .set_delay(Duration::from_secs(3)),
    )
    .await;
    let qs = questions();
    let error = client(
        Provider::Laya,
        &server.uri(),
        "multilingual",
        Duration::from_secs(1),
    )
    .decide(
        &DecisionRequest {
            title: "t",
            body: "b",
            questions: &qs,
        },
        deadline(),
    )
    .await
    .result
    .unwrap_err();
    assert_eq!(error, DecisionError::Timeout { seconds: 1 });
}

#[tokio::test]
async fn invalid_requests_never_reach_the_network() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let jev = client(
        Provider::Jev,
        &server.uri(),
        "jev-1.13.0",
        Duration::from_secs(5),
    );
    let empty = jev
        .decide(
            &DecisionRequest {
                title: "t",
                body: "b",
                questions: &[],
            },
            deadline(),
        )
        .await
        .result;
    assert!(matches!(empty, Err(DecisionError::InvalidRequest(_))));
    let eleven = [QuestionSpec {
        id: "q_level".to_string(),
        instructions: "x".to_string(),
        kind: QuestionKind::Score {
            levels: (0..11).map(|level| level.to_string()).collect(),
        },
    }];
    let too_many = jev
        .decide(
            &DecisionRequest {
                title: "t",
                body: "b",
                questions: &eleven,
            },
            deadline(),
        )
        .await
        .result;
    assert!(matches!(too_many, Err(DecisionError::InvalidRequest(_))));
}

#[tokio::test]
async fn construction_rejects_bad_settings() {
    let settings = |provider, model: &str| ClientSettings {
        provider,
        base_url: Url::parse("http://127.0.0.1:9").unwrap(),
        model: model.to_string(),
        request_timeout: Duration::from_secs(1),
        max_state_chars: 10,
        laya_max_len: 2048,
        use_env_proxy: false,
    };
    assert!(
        SystemOneClient::new(settings(Provider::Laya, "auto"), SecretString::from(KEY)).is_err()
    );
    assert!(
        SystemOneClient::new(
            settings(Provider::Jev, "multilingual"),
            SecretString::from(KEY)
        )
        .is_err()
    );
    assert!(
        SystemOneClient::new(
            settings(Provider::Laya, "multilingual"),
            SecretString::from(" ")
        )
        .is_err()
    );
    let mut with_userinfo = settings(Provider::Laya, "multilingual");
    with_userinfo.base_url = Url::parse("http://user:pass@127.0.0.1:9").unwrap();
    assert!(SystemOneClient::new(with_userinfo, SecretString::from(KEY)).is_err());
}

#[tokio::test]
async fn unreachable_endpoint_is_a_connection_error() {
    // Bound but never listening: Linux refuses the connect, and holding the
    // socket keeps any other process from taking the port meanwhile.
    let reserved = TcpSocket::new_v4().unwrap();
    reserved.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = reserved.local_addr().unwrap();
    let qs = questions();
    let error = client(
        Provider::Laya,
        &format!("http://{address}"),
        "multilingual",
        Duration::from_secs(5),
    )
    .decide(
        &DecisionRequest {
            title: "t",
            body: "b",
            questions: &qs,
        },
        deadline(),
    )
    .await
    .result
    .unwrap_err();
    assert_eq!(error, DecisionError::Connection);
    drop(reserved);
}

/// One Laya exchange against `template`, with the full `Exchange` returned.
async fn laya_exchange(template: ResponseTemplate) -> (MockServer, Exchange) {
    let server = serve(template).await;
    let qs = questions();
    let exchange = client(
        Provider::Laya,
        &server.uri(),
        "multilingual",
        Duration::from_secs(5),
    )
    .decide(
        &DecisionRequest {
            title: "标题",
            body: "正文",
            questions: &qs,
        },
        deadline(),
    )
    .await;
    (server, exchange)
}

#[tokio::test]
async fn exchange_reports_what_was_sent_and_captures_a_clean_body() {
    let (server, exchange) =
        laya_exchange(ResponseTemplate::new(200).set_body_json(laya_ok())).await;
    exchange.result.unwrap();
    let sent = exchange.sent.unwrap();
    let state = sent_body(&server).await["state"].to_string();
    assert_eq!(
        sent.state_sha256,
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(state.as_bytes()))
    );
    assert_eq!((sent.body_chars, sent.body_truncated), (2, false));
    let raw = exchange.raw.unwrap();
    let body = raw.body.clone().unwrap();
    assert_eq!(raw.bytes, body.len() as u64);
    assert_eq!(raw.withheld, None);
    assert_eq!(
        raw.sha256.as_deref(),
        Some(hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&body)).as_str())
    );
    assert!(!format!("{raw:?}").contains("laya-rl-agent"));
}

#[tokio::test]
async fn non_json_and_credential_bearing_bodies_are_withheld() {
    let (_server, exchange) =
        laya_exchange(ResponseTemplate::new(200).set_body_raw("<html>ok</html>", "text/html"))
            .await;
    assert!(matches!(
        exchange.result,
        Err(DecisionError::InvalidResponse { .. })
    ));
    let raw = exchange.raw.unwrap();
    assert_eq!(
        (raw.withheld, raw.body.is_some(), raw.bytes),
        (Some(Withheld::NotJson), false, 15)
    );
    assert!(raw.sha256.is_some());

    // Escaped inside an ignored field: the answer stands, the body is withheld.
    let escaped: String = KEY
        .chars()
        .map(|c| format!("\\u{:04x}", c as u32))
        .collect();
    let text = laya_ok().to_string().replacen(
        "\"legend\":{",
        &format!("\"legend\":{{\"note\":\"{escaped}\","),
        1,
    );
    let (_server, exchange) =
        laya_exchange(ResponseTemplate::new(200).set_body_raw(text, "application/json")).await;
    exchange.result.unwrap();
    assert_eq!(exchange.raw.unwrap().withheld, Some(Withheld::Credential));

    // In the stored model name: the whole response is rejected.
    let mut reflected = laya_ok();
    reflected["model"] = json!(format!("laya-{KEY}"));
    let (_server, exchange) =
        laya_exchange(ResponseTemplate::new(200).set_body_json(reflected)).await;
    assert_invalid(exchange.result, "reflects the credential");
    assert_eq!(exchange.raw.unwrap().withheld, Some(Withheld::Credential));
}

#[tokio::test]
async fn a_broken_off_or_oversized_body_is_incomplete_without_a_digest() {
    const BROKEN_200: &str = "HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n{\"model\":";
    let qs = questions();
    let exchange = client(
        Provider::Laya,
        &raw_server(BROKEN_200, false).await,
        "multilingual",
        Duration::from_secs(1),
    )
    .decide(
        &DecisionRequest {
            title: "t",
            body: "b",
            questions: &qs,
        },
        deadline(),
    )
    .await;
    assert_eq!(exchange.result.unwrap_err(), DecisionError::Transport);
    let raw = exchange.raw.unwrap();
    assert_eq!(
        (raw.withheld, raw.sha256, raw.bytes),
        (Some(Withheld::Incomplete), None, 9)
    );
}

#[tokio::test]
async fn the_deadline_bounds_the_call_and_keeps_a_known_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let qs = questions();
    let request = DecisionRequest {
        title: "t",
        body: "b",
        questions: &qs,
    };
    let exchange = client(
        Provider::Laya,
        &server.uri(),
        "multilingual",
        Duration::from_secs(5),
    )
    .decide(&request, std::time::Instant::now())
    .await;
    assert_eq!(
        exchange.result.unwrap_err(),
        DecisionError::Timeout { seconds: 0 }
    );
    assert!(exchange.sent.is_none());

    const STALLED_401: &str =
        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 1000\r\n\r\n{\"detail\":";
    let base = raw_server(STALLED_401, true).await;
    let exchange = client(
        Provider::Laya,
        &base,
        "multilingual",
        Duration::from_secs(30),
    )
    .decide(&request, std::time::Instant::now() + Duration::from_secs(1))
    .await;
    assert_eq!(
        exchange.result.unwrap_err(),
        DecisionError::Unauthorized { status: 401 }
    );
}

#[tokio::test]
async fn a_dropped_connection_is_sent_once_and_never_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buffer = [0_u8; 4096];
            let read = socket.read(&mut buffer).await.expect("read the request");
            assert!(read > 0, "the client sends a request before the close");
            // Close without any response.
        }
    });
    let qs = questions();
    let error = client(
        Provider::Laya,
        &format!("http://{address}"),
        "multilingual",
        Duration::from_secs(2),
    )
    .decide(
        &DecisionRequest {
            title: "t",
            body: "b",
            questions: &qs,
        },
        deadline(),
    )
    .await
    .result
    .unwrap_err();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(error, DecisionError::Transport);
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_key_that_is_not_a_header_value_is_a_config_error() {
    let settings = ClientSettings {
        provider: Provider::Laya,
        base_url: Url::parse("http://127.0.0.1:9").unwrap(),
        model: "multilingual".to_string(),
        request_timeout: Duration::from_secs(1),
        max_state_chars: 10,
        laya_max_len: 2048,
        use_env_proxy: false,
    };
    let error = SystemOneClient::new(settings, SecretString::from(format!("{KEY}\n"))).unwrap_err();
    assert!(matches!(error, DecisionError::InvalidConfig(_)));
    assert!(!error.to_string().contains(KEY));
}

#[tokio::test]
async fn a_stored_body_revalidates_offline_to_the_same_result() {
    let (_server, exchange) =
        laya_exchange(ResponseTemplate::new(200).set_body_json(laya_ok())).await;
    let online = exchange.result.unwrap();
    let body = exchange.raw.unwrap().body.unwrap();
    let policy = canonical_policy(&questions(), 6000);
    let offline = revalidate(Provider::Laya, "multilingual", Some(2048), &policy, &body).unwrap();
    assert_eq!(offline, online);
    let mut tampered = laya_ok();
    tampered["answers"]["q_kind"]["choice"] = json!("other");
    let tampered = tampered.to_string();
    assert_invalid(
        revalidate(
            Provider::Laya,
            "multilingual",
            Some(2048),
            &policy,
            tampered.as_bytes(),
        ),
        "most probable",
    );
    assert!(matches!(
        revalidate(Provider::Laya, "multilingual", Some(2048), "{}", &body),
        Err(DecisionError::InvalidConfig(_))
    ));
}
