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

#[tokio::test]
async fn invoke_rejects_oversized_success_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": "x".repeat(4 * 1024 * 1024) } }]
        })))
        .mount(&server)
        .await;
    let err = match test_client(server.uri()).invoke(&test_task()).await {
        Err(error) => error,
        Ok(_) => panic!("response must be bounded"),
    };
    assert_eq!(err.error_kind(), "response_too_large");
    assert!(!err.is_retryable());
    assert!(err.should_fallback());
}

#[tokio::test]
async fn invoke_redacts_provider_echoed_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "message": "Incorrect API key provided: sk-test" }
        })))
        .mount(&server)
        .await;
    let err = test_client(server.uri())
        .invoke(&test_task())
        .await
        .expect_err("401 fails");
    assert!(!format!("{err:?} {err}").contains("sk-test"));
}

#[tokio::test]
async fn invoke_redacts_json_escaped_provider_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_raw(
            r#"{"error":{"message":"Incorrect API key: \u0073k-test"}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let error = test_client(server.uri())
        .invoke(&test_task())
        .await
        .expect_err("401 fails");
    assert!(!format!("{error:?} {error}").contains("sk-test"));
    assert!(error.display_user().contains("***"));
}

#[test]
fn client_config_debug_hides_url_credentials_and_query() {
    let config = rss_ai_news_ai::AiClientConfig {
        api_base: "https://user:password@example.test/v1?key=query-secret".into(),
        api_key: "sk-test".into(),
        request_timeout: Duration::from_secs(2),
    };
    let client = rss_ai_news_ai::OpenAiCompatClient::new(config.clone()).expect("valid config");
    let debug = format!("{config:?} {client:?}");
    for secret in ["password", "query-secret", "sk-test"] {
        assert!(!debug.contains(secret), "Debug leaked {secret}");
    }
}

// A raw local HTTP fixture is intentional: wiremock supplies Content-Length,
// so it cannot prove that the reader enforces its cap while streaming.
fn chunked_server(body: Vec<u8>, delay: Duration) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking accept");
    let address = listener.local_addr().expect("fixture address");
    let handle = std::thread::spawn(move || {
        let accept_started = std::time::Instant::now();
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        accept_started.elapsed() < Duration::from_secs(3),
                        "fixture received no connection"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept fixture connection: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("write timeout");
        let mut reader = std::io::BufReader::new(&mut stream);
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("request header") == 0 {
                return; // Request deadline may fire before headers reach the fixture.
            }
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse::<u64>().expect("content length");
            }
        }
        std::io::copy(&mut reader.take(content_length), &mut std::io::sink())
            .expect("request body");
        let mut send = || -> std::io::Result<()> {
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )?;
            std::thread::sleep(delay);
            for chunk in body.chunks(8192) {
                write!(stream, "{:x}\r\n", chunk.len())?;
                stream.write_all(chunk)?;
                stream.write_all(b"\r\n")?;
            }
            stream.write_all(b"0\r\n\r\n")
        };
        // The client deliberately closes early on a size limit or timeout.
        if let Err(err) = send() {
            assert!(matches!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ));
        }
    });
    (format!("http://{address}"), handle)
}

#[tokio::test]
async fn invoke_limits_chunked_response_without_content_length() {
    const LIMIT: usize = 4 * 1024 * 1024;
    let small = br#"{"choices":[{"message":{"content":"ok"}}]}"#;
    for size in [small.len(), LIMIT, LIMIT + 1] {
        let mut body = small.to_vec();
        body.resize(size, b' '); // JSON allows trailing whitespace.
        let (url, server) = chunked_server(body, Duration::ZERO);
        let result = test_client(url).invoke(&test_task()).await;
        server.join().expect("fixture thread");
        if size <= LIMIT {
            assert_eq!(result.expect("body at or below limit").raw_response, "ok");
        } else {
            assert_eq!(
                result.expect_err("streamed body above limit").error_kind(),
                "response_too_large"
            );
        }
    }
}

#[tokio::test]
async fn invoke_timeout_includes_streaming_body() {
    let (url, server) = chunked_server(b"{}".to_vec(), Duration::from_millis(750));
    let err = test_client_with_timeout(url, Duration::from_millis(500))
        .invoke(&test_task())
        .await
        .expect_err("body deadline must apply");
    server.join().expect("fixture thread");
    assert!(matches!(err, AiError::HttpTimeout { .. }));
}

#[tokio::test]
async fn oversized_error_body_preserves_retryable_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("x".repeat(4 * 1024 * 1024 + 1)))
        .mount(&server)
        .await;
    let err = test_client(server.uri())
        .invoke(&test_task())
        .await
        .expect_err("503 fails");
    assert!(matches!(err, AiError::HttpStatus { code: 503, .. }));
    assert!(err.is_retryable());
    assert!(
        err.to_string().len() < 200,
        "oversized body must not enter logs or storage"
    );
}

#[tokio::test]
async fn invoke_classifies_quota_and_missing_model_on_real_request_path() {
    for (status, code, expected_kind) in [
        (429, "insufficient_quota", "quota_exceeded"),
        (404, "model_not_found", "model_unavailable"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "error": { "message": "provider failure", "code": code }
            })))
            .mount(&server)
            .await;
        let err = test_client(server.uri())
            .invoke(&test_task())
            .await
            .expect_err("provider error");
        assert_eq!(err.error_kind(), expected_kind);
        assert!(!err.is_retryable());
        assert!(err.should_fallback());
    }
}

#[tokio::test]
async fn provider_diagnostics_are_bounded_after_decoding_and_redaction() {
    for (status, kind) in [
        (503, "http_status"),
        (429, "quota_exceeded"),
        (404, "model_unavailable"),
    ] {
        for structured in [false, true] {
            let server = MockServer::start().await;
            let tail = match status {
                429 => " quota",
                404 => " model not found",
                _ => "",
            };
            let message = format!("sk-test {}{tail}", "中文🦀".repeat(200_000));
            let body = if structured {
                json!({"error":{"message":message}}).to_string()
            } else {
                message
            };
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            let error = test_client(server.uri())
                .invoke(&test_task())
                .await
                .unwrap_err();
            assert_eq!(error.error_kind(), kind);
            for diagnostic in [error.to_string(), error.display_user()] {
                assert!(
                    diagnostic.len() <= 16 * 1024,
                    "diagnostic used {} bytes",
                    diagnostic.len()
                );
                assert!(diagnostic.contains("[truncated, original_bytes="));
                assert!(!diagnostic.contains("sk-test"));
            }
        }
    }
}
