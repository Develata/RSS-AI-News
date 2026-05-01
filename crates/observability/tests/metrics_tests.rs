use rss_ai_news_observability::{InMemoryMetrics, MetricsRecorder, NullMetrics};

#[test]
fn null_metrics_is_noop() {
    let metrics = NullMetrics;
    metrics.counter_inc("requests", &[("status", "ok")], 1);
    metrics.histogram_observe("latency", &[], 0.1);
    metrics.gauge_set("active", &[], 3.0);
}

#[test]
fn in_memory_metrics_records_counters_histograms_and_gauges() {
    let metrics = InMemoryMetrics::default();
    metrics.counter_inc("requests", &[("status", "ok")], 2);
    metrics.counter_inc("requests", &[("status", "ok")], 3);
    metrics.histogram_observe("latency", &[("route", "/")], 0.25);
    metrics.histogram_observe("latency", &[("route", "/")], 0.5);
    metrics.gauge_set("active", &[("queue", "ai")], 7.0);

    assert_eq!(metrics.counter_total("requests", &[("status", "ok")]), 5);
    assert_eq!(
        metrics.histogram_samples("latency", &[("route", "/")]),
        vec![0.25, 0.5]
    );
    assert_eq!(metrics.gauge_value("active", &[("queue", "ai")]), Some(7.0));
}
