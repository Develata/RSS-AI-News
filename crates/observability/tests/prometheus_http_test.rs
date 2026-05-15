use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use rss_ai_news_observability::{MetricsRecorder, PrometheusMetrics, serve_metrics};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// F15-14 W9-F2：端到端验证 `/metrics` HTTP 端点能被 prometheus scraper
/// 风格的 GET 请求消费——counter_inc 后启动 server、HTTP GET、解析响应
/// 的 status line + 响应体确认 metric 暴露。
#[tokio::test]
async fn metrics_endpoint_serves_counter_in_prometheus_text_format() {
    let recorder = Arc::new(PrometheusMetrics::new());
    recorder.counter_inc("e2e_total", &[("kind", "demo")], 7);

    // 用 0 端口让 OS 分配空闲端口，避免 CI 上端口冲突。先 bind 拿到地址，
    // 再 spawn server 复用同 listener？serve_metrics 内部 bind，需另一种
    // 拿地址方式。这里改用 spawn + 自旋等端口绑定成功。
    //
    // 简化：循环尝试 50_000~50_010 端口，第一个 bind 成功即用。
    let mut server_addr: Option<SocketAddr> = None;
    let mut server_task = None;
    for port in 50_000u16..50_100 {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        // 用 TcpListener 探测端口可用性，再立即关闭、交给 serve_metrics 用
        // 同地址重新 bind。理论上有 TOCTOU；在测试场景下风险可接受。
        if std::net::TcpListener::bind(addr).is_ok() {
            let recorder_clone = recorder.clone();
            server_task = Some(tokio::spawn(async move {
                let _ = serve_metrics(addr, recorder_clone).await;
            }));
            server_addr = Some(addr);
            break;
        }
    }
    let addr = server_addr.expect("应当找到至少一个可用端口");
    let _task = server_task.expect("serve_metrics 应已 spawn");

    // 等 server 真正进入 accept loop（轮询 connect 直到成功 / 1s 超时）
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut socket = loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("server 未就绪：{error}"),
        }
    };

    socket
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::with_capacity(4096);
    socket.read_to_end(&mut buf).await.unwrap();
    let response = String::from_utf8_lossy(&buf);
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "应当 200：{response}"
    );
    assert!(response.contains("Content-Type: text/plain"));
    assert!(
        response.contains("e2e_total{kind=\"demo\"} 7"),
        "响应体应当包含 counter line：{response}"
    );
}

#[tokio::test]
async fn metrics_endpoint_returns_404_for_unknown_path() {
    let recorder = Arc::new(PrometheusMetrics::new());
    let mut server_addr: Option<SocketAddr> = None;
    let mut server_task = None;
    for port in 50_200u16..50_300 {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        if std::net::TcpListener::bind(addr).is_ok() {
            let recorder_clone = recorder.clone();
            server_task = Some(tokio::spawn(async move {
                let _ = serve_metrics(addr, recorder_clone).await;
            }));
            server_addr = Some(addr);
            break;
        }
    }
    let addr = server_addr.expect("应当找到至少一个可用端口");
    let _task = server_task.unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    let mut socket = loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("server 未就绪：{error}"),
        }
    };
    socket
        .write_all(b"GET /other HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::with_capacity(1024);
    socket.read_to_end(&mut buf).await.unwrap();
    let response = String::from_utf8_lossy(&buf);
    assert!(
        response.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "未知路径应当 404：{response}"
    );
}
