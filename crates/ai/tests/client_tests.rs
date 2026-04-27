mod common;

use std::time::Duration;

use rss_ai_news_ai::{AiClient, AiError};
use rss_ai_news_domain::error::ClassifiedError;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

use common::{test_client, test_client_with_timeout, test_task};

#[tokio::test]
async fn invoke_returns_response_on_200_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "{\"summary\":\"ok\",\"tags\":[],\"importance_score\":80,\"keep_decision\":true}"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "total_tokens": 20
            }
        })))
        .mount(&server)
        .await;

    let client = test_client(server.uri());
    let client_debug = format!("{client:?}");
    assert!(!client_debug.contains("sk-test"));
    let response = client.invoke(&test_task()).await.expect("200 succeeds");

    assert_eq!(response.article_ai_result_id, 7);
    assert_eq!(
        response.raw_response,
        "{\"summary\":\"ok\",\"tags\":[],\"importance_score\":80,\"keep_decision\":true}"
    );
    let usage = response.usage.expect("usage should be mapped");
    assert_eq!(usage.tokens_in, 12);
    assert_eq!(usage.tokens_out, 8);
    assert_eq!(usage.cost_micro_usd, None);
}

#[tokio::test]
async fn invoke_returns_rate_limited_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_json(json!({
                    "error": {
                        "message": "rate limit exceeded",
                        "type": "rate_limit_error",
                        "code": "rate_limit_exceeded"
                    }
                })),
        )
        .mount(&server)
        .await;

    let client = test_client(server.uri());
    let err = client.invoke(&test_task()).await.expect_err("429 fails");

    assert!(matches!(
        err,
        AiError::RateLimited {
            retry_after_seconds: Some(3),
            ..
        }
    ));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn invoke_returns_retryable_http_status_on_503() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": {
                "message": "service unavailable",
                "type": "server_error",
                "code": null
            }
        })))
        .mount(&server)
        .await;

    let client = test_client(server.uri());
    let err = client.invoke(&test_task()).await.expect_err("503 fails");

    assert!(matches!(err, AiError::HttpStatus { code: 503, .. }));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn invoke_returns_permanent_http_status_on_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "message": "bad request",
                "type": "invalid_request_error",
                "code": null
            }
        })))
        .mount(&server)
        .await;

    let client = test_client(server.uri());
    let err = client.invoke(&test_task()).await.expect_err("400 fails");

    assert!(matches!(err, AiError::HttpStatus { code: 400, .. }));
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn invoke_returns_http_timeout_when_server_slow() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!({
                    "choices": [{
                        "message": {
                            "content": "{\"summary\":\"late\",\"tags\":[],\"importance_score\":1,\"keep_decision\":true}"
                        }
                    }]
                })),
        )
        .mount(&server)
        .await;

    let client = test_client_with_timeout(server.uri(), Duration::from_millis(30));
    let err = client
        .invoke(&test_task())
        .await
        .expect_err("slow server times out");

    assert!(matches!(err, AiError::HttpTimeout { .. }));
    assert!(err.is_retryable());
}
