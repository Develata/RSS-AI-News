//! Prometheus exporter（F15-14 W9-F2）。
//!
//! 通过 [`prometheus`] crate 把 [`MetricsRecorder`](crate::MetricsRecorder)
//! 的 counter / histogram / gauge 调用转译成 prometheus 标准格式，再用
//! 最小 tokio TcpListener HTTP/1.1 服务挂到 `metrics_bind`，被
//! prometheus / Grafana Agent / VictoriaMetrics 拉取。
//!
//! ## 设计取舍
//!   - 用 [`prometheus`] 0.13 而非 metrics-rs 系列：前者是 Rust 社区
//!     prometheus 标准实现，单进程模型直接对齐我们的 CLI 进程模型；
//!     metrics-rs 偏好长生命周期 daemon 模型
//!   - 标签集合**动态**注册：counter / histogram 首次出现时按 label keys
//!     注册 `*Vec`；同名指标后续 label key 变化会拒绝（防止指标爆炸）
//!   - HTTP 服务用裸 tokio TCP：避免引入 hyper / axum 这种数千行依赖；
//!     `/metrics` 是 1 个 GET 端点，请求/响应模板极短，自己处理足够
//!
//! ## 已知边界
//!   - CLI startup 在 config.toml 读取之前，所以 `[observability]`
//!     字段（`enable_metrics`、`metrics_bind`）当前必须经 `--metrics-bind`
//!     标志透传。与 F15-13 `--log-file` 同源限制
//!   - prometheus 命名规范：metric name 必须匹配 `[a-zA-Z_:][a-zA-Z0-9_:]*`；
//!     label name 同 metric name 规则。本模块**不**做硬校验，prometheus
//!     crate 的 `register_*` 会在不合法时返 Err；调用方代码里出现的名字
//!     是 `&'static str`，PR review 时人工把关

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use prometheus::{
    CounterVec, HistogramOpts, HistogramVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinSet,
};

use crate::metrics::MetricsRecorder;

/// prometheus::Registry 的可观测包装。
///
/// 通过 `MetricsRecorder` trait 暴露给业务代码——counter_inc 等调用以
/// `(&'static name, &[(label, value)])` 形式进入，本结构按 label keys 在
/// `metrics_*` map 中索引到对应的 `*Vec`，再 `with_label_values` 取
/// 具体 child。
///
/// 同名 metric 不允许在不同调用点使用不同 label keys——这与 prometheus
/// 模型矛盾（同 metric 的所有 series 必须共享 label keys）。本实现遇到
/// 不一致 key 会**静默回退**到 `NullMetrics` 行为：丢弃该次观测并发
/// `tracing::warn!`。理由：metric 观测不应让请求路径 panic 或阻塞。
pub struct PrometheusMetrics {
    registry: Registry,
    counters: Mutex<HashMap<&'static str, RegisteredCounter>>,
    histograms: Mutex<HashMap<&'static str, RegisteredHistogram>>,
    gauges: Mutex<HashMap<&'static str, RegisteredGauge>>,
}

struct RegisteredCounter {
    vec: CounterVec,
    label_keys: Vec<String>,
}

struct RegisteredHistogram {
    vec: HistogramVec,
    label_keys: Vec<String>,
}

struct RegisteredGauge {
    vec: IntGaugeVec,
    label_keys: Vec<String>,
}

impl PrometheusMetrics {
    pub fn new() -> Self {
        Self {
            registry: Registry::new(),
            counters: Mutex::new(HashMap::new()),
            histograms: Mutex::new(HashMap::new()),
            gauges: Mutex::new(HashMap::new()),
        }
    }

    /// 渲染当前所有指标为 prometheus 文本格式（exposition format v0.0.4）。
    /// 直接当作 `/metrics` HTTP 端点的 body 返回。
    pub fn render(&self) -> String {
        let metric_families = self.registry.gather();
        let mut buffer = String::new();
        let encoder = TextEncoder::new();
        if let Err(error) = encoder.encode_utf8(&metric_families, &mut buffer) {
            // encode 失败极罕见（仅 utf-8 边界问题）；返一段可被 prometheus
            // 兼容的 comment 行让 scrape 端能看到症状。
            buffer.clear();
            buffer.push_str(&format!(
                "# encode_utf8 failed: {error}\n# HELP prom_render_error 1\n# TYPE prom_render_error counter\nprom_render_error 1\n"
            ));
        }
        buffer
    }

    fn ordered_labels<'a>(labels: &'a [(&'a str, &'a str)]) -> (Vec<String>, Vec<&'a str>) {
        let mut pairs = labels.to_vec();
        pairs.sort_by(|left, right| left.0.cmp(right.0));
        let keys = pairs.iter().map(|pair| pair.0.to_string()).collect();
        let values = pairs.iter().map(|pair| pair.1).collect();
        (keys, values)
    }
}

impl Default for PrometheusMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsRecorder for PrometheusMetrics {
    fn counter_inc(&self, name: &'static str, labels: &[(&str, &str)], value: u64) {
        let (keys, values) = Self::ordered_labels(labels);
        let mut guard = self
            .counters
            .lock()
            .expect("prometheus counters mutex poisoned");
        let entry = guard.entry(name).or_insert_with(|| {
            let label_refs: Vec<&str> = keys.iter().map(|key| key.as_str()).collect();
            let vec = CounterVec::new(Opts::new(name, name), &label_refs)
                .expect("counter spec should be valid");
            if let Err(error) = self.registry.register(Box::new(vec.clone())) {
                tracing::warn!(metric = %name, ?error, "counter register failed");
            }
            RegisteredCounter {
                vec,
                label_keys: keys.clone(),
            }
        });
        if entry.label_keys != keys {
            tracing::warn!(
                metric = %name,
                expected = ?entry.label_keys,
                actual = ?keys,
                "counter label keys mismatch — observation dropped"
            );
            return;
        }
        entry.vec.with_label_values(&values).inc_by(value as f64);
    }

    fn histogram_observe(&self, name: &'static str, labels: &[(&str, &str)], value: f64) {
        let (keys, values) = Self::ordered_labels(labels);
        let mut guard = self
            .histograms
            .lock()
            .expect("prometheus histograms mutex poisoned");
        let entry = guard.entry(name).or_insert_with(|| {
            let label_refs: Vec<&str> = keys.iter().map(|key| key.as_str()).collect();
            // 默认 bucket：覆盖 ms~分钟 量级延迟与 0~10k 计数量级；
            // 后续业务指标需要不同 bucket 时可以新增 named-bucket API。
            let opts = HistogramOpts::new(name, name).buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ]);
            let vec = HistogramVec::new(opts, &label_refs).expect("histogram spec should be valid");
            if let Err(error) = self.registry.register(Box::new(vec.clone())) {
                tracing::warn!(metric = %name, ?error, "histogram register failed");
            }
            RegisteredHistogram {
                vec,
                label_keys: keys.clone(),
            }
        });
        if entry.label_keys != keys {
            tracing::warn!(
                metric = %name,
                expected = ?entry.label_keys,
                actual = ?keys,
                "histogram label keys mismatch — observation dropped"
            );
            return;
        }
        entry.vec.with_label_values(&values).observe(value);
    }

    fn gauge_set(&self, name: &'static str, labels: &[(&str, &str)], value: f64) {
        let (keys, values) = Self::ordered_labels(labels);
        let mut guard = self
            .gauges
            .lock()
            .expect("prometheus gauges mutex poisoned");
        let entry = guard.entry(name).or_insert_with(|| {
            let label_refs: Vec<&str> = keys.iter().map(|key| key.as_str()).collect();
            let vec = IntGaugeVec::new(Opts::new(name, name), &label_refs)
                .expect("gauge spec should be valid");
            if let Err(error) = self.registry.register(Box::new(vec.clone())) {
                tracing::warn!(metric = %name, ?error, "gauge register failed");
            }
            RegisteredGauge {
                vec,
                label_keys: keys.clone(),
            }
        });
        if entry.label_keys != keys {
            tracing::warn!(
                metric = %name,
                expected = ?entry.label_keys,
                actual = ?keys,
                "gauge label keys mismatch — observation dropped"
            );
            return;
        }
        // gauge_set 接 f64，prometheus IntGauge 用 i64：四舍五入；
        // 业务侧目前的 gauge 值（lease 余量、in-flight 计数）天然整型。
        entry.vec.with_label_values(&values).set(value as i64);
    }
}

/// Serve `/metrics` with at most 32 active connections and a five-second total
/// read/write deadline. Dropping the server cancels every connection handler.
pub async fn serve_metrics(bind: SocketAddr, recorder: Arc<PrometheusMetrics>) -> io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(addr = %bind, "prometheus /metrics endpoint listening");
    serve_metrics_listener(listener, recorder, 32, Duration::from_secs(5)).await
}

async fn serve_metrics_listener(
    listener: TcpListener,
    recorder: Arc<PrometheusMetrics>,
    max_connections: usize,
    request_timeout: Duration,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::warn!(?error, "metrics connection task failed");
                }
            }
            accepted = listener.accept(), if connections.len() < max_connections => {
                let (socket, peer) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::warn!(?error, "metrics listener accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let recorder = recorder.clone();
                connections.spawn(async move {
                    match tokio::time::timeout(request_timeout, handle_metrics_connection(socket, recorder)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(?error, %peer, "metrics connection handler failed"),
                        Err(_) => tracing::warn!(%peer, "metrics connection timed out"),
                    }
                });
            }
        }
    }
}

async fn handle_metrics_connection(
    mut socket: tokio::net::TcpStream,
    recorder: Arc<PrometheusMetrics>,
) -> io::Result<()> {
    let mut buf = [0u8; 4096];
    let mut used = 0;
    let headers_end = loop {
        let read = socket.read(&mut buf[used..]).await?;
        if read == 0 {
            return Ok(());
        }
        // Include the previous three bytes to find a delimiter split across TCP reads.
        let search_from = used.saturating_sub(3);
        used += read;
        if let Some(end) = buf[search_from..used]
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
        {
            break search_from + end + 4;
        }
        if used == buf.len() {
            socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            socket.shutdown().await?;
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..headers_end]);
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method == "GET" && path == "/metrics" && matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        let body = recorder.render();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        );
        socket.write_all(header.as_bytes()).await?;
        socket.write_all(body.as_bytes()).await?;
    } else {
        socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nNot Found\n").await?;
    }
    socket.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_inc_renders_to_prometheus_text() {
        let metrics = PrometheusMetrics::new();
        metrics.counter_inc("ingest_total", &[("category", "ai")], 3);
        metrics.counter_inc("ingest_total", &[("category", "ai")], 2);
        let text = metrics.render();
        assert!(
            text.contains("ingest_total"),
            "render 应包含 metric name：\n{text}"
        );
        assert!(
            text.contains("category=\"ai\""),
            "render 应包含 label：\n{text}"
        );
        // counter 累积为 5
        assert!(
            text.lines()
                .any(|line| line.starts_with("ingest_total{") && line.ends_with(" 5")),
            "counter 累积值应为 5：\n{text}"
        );
    }

    #[test]
    fn histogram_observe_renders_buckets_and_count() {
        let metrics = PrometheusMetrics::new();
        metrics.histogram_observe("ingest_duration_seconds", &[("stage", "fetch")], 0.04);
        metrics.histogram_observe("ingest_duration_seconds", &[("stage", "fetch")], 0.3);
        let text = metrics.render();
        assert!(text.contains("ingest_duration_seconds_bucket"));
        assert!(text.contains("ingest_duration_seconds_count"));
        assert!(text.contains("ingest_duration_seconds_sum"));
    }

    #[test]
    fn gauge_set_uses_latest_value() {
        let metrics = PrometheusMetrics::new();
        metrics.gauge_set("in_flight", &[], 5.0);
        metrics.gauge_set("in_flight", &[], 7.0);
        let text = metrics.render();
        assert!(
            text.lines()
                .any(|line| line.starts_with("in_flight") && line.ends_with(" 7")),
            "gauge 应取最近一次 set：\n{text}"
        );
    }

    #[test]
    fn label_keys_mismatch_drops_observation_silently() {
        // 同 metric 第二次出现不同 label keys → observation 丢弃、不 panic。
        // counter 仍保持第一次注册的 label set。
        let metrics = PrometheusMetrics::new();
        metrics.counter_inc("conflict_metric", &[("a", "1")], 1);
        metrics.counter_inc("conflict_metric", &[("b", "2")], 9);
        let text = metrics.render();
        assert!(text.contains("a=\"1\""));
        assert!(!text.contains("b=\"2\""));
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    async fn connection() -> (
        tokio::net::TcpStream,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            handle_metrics_connection(socket, Arc::new(PrometheusMetrics::new())).await
        });
        (tokio::net::TcpStream::connect(address).await.unwrap(), task)
    }

    #[tokio::test]
    async fn fragmented_request_is_read_until_headers_are_complete() {
        let (mut socket, task) = connection().await;
        socket.write_all(b"GET /met").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        socket
            .write_all(b"rics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn oversized_headers_are_rejected() {
        let (mut socket, task) = connection().await;
        socket.write_all(&[b'a'; 4096]).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response}"
        );
        task.await.unwrap().unwrap();
    }

    async fn server(
        max_connections: usize,
        timeout: Duration,
    ) -> (SocketAddr, tokio::task::JoinHandle<io::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_metrics_listener(
            listener,
            Arc::new(PrometheusMetrics::new()),
            max_connections,
            timeout,
        ));
        (address, task)
    }

    #[tokio::test]
    async fn idle_connection_expires() {
        let (address, task) = server(2, Duration::from_millis(100)).await;
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        task.abort();
    }

    #[tokio::test]
    async fn dropping_server_cancels_active_connections() {
        let (address, task) = server(2, Duration::from_secs(10)).await;
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(b"GET /met").await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let mut byte = [0];
        let result = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
            .await
            .unwrap();
        assert!(
            matches!(result, Ok(0)) || result.is_err(),
            "server left a detached connection: {result:?}"
        );
    }

    #[tokio::test]
    async fn connections_wait_for_a_free_handler_slot() {
        let (address, task) = server(1, Duration::from_secs(5)).await;
        let mut first = tokio::net::TcpStream::connect(address).await.unwrap();
        first.write_all(b"GET /met").await.unwrap();
        // One incomplete request consumes the sole slot; the next remains in
        // the kernel accept backlog until that handler releases its resources.
        let mut second = tokio::net::TcpStream::connect(address).await.unwrap();
        second
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                second.read_to_string(&mut response)
            )
            .await
            .is_err()
        );
        drop(first);
        tokio::time::timeout(Duration::from_secs(2), second.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        task.abort();
    }
}
