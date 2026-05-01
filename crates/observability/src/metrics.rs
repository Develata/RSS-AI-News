use std::{collections::HashMap, sync::Mutex};

pub trait MetricsRecorder: Send + Sync {
    fn counter_inc(&self, name: &'static str, labels: &[(&str, &str)], value: u64);
    fn histogram_observe(&self, name: &'static str, labels: &[(&str, &str)], value: f64);
    fn gauge_set(&self, name: &'static str, labels: &[(&str, &str)], value: f64);
}

#[derive(Debug, Default)]
pub struct NullMetrics;

impl MetricsRecorder for NullMetrics {
    fn counter_inc(&self, _: &'static str, _: &[(&str, &str)], _: u64) {}
    fn histogram_observe(&self, _: &'static str, _: &[(&str, &str)], _: f64) {}
    fn gauge_set(&self, _: &'static str, _: &[(&str, &str)], _: f64) {}
}

#[derive(Debug, Default)]
pub struct InMemoryMetrics {
    counters: Mutex<HashMap<String, u64>>,
    histograms: Mutex<HashMap<String, Vec<f64>>>,
    gauges: Mutex<HashMap<String, f64>>,
}

impl InMemoryMetrics {
    pub fn counter_total(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        self.counters
            .lock()
            .expect("metrics mutex poisoned")
            .get(&key(name, labels))
            .copied()
            .unwrap_or(0)
    }

    pub fn histogram_samples(&self, name: &str, labels: &[(&str, &str)]) -> Vec<f64> {
        self.histograms
            .lock()
            .expect("metrics mutex poisoned")
            .get(&key(name, labels))
            .cloned()
            .unwrap_or_default()
    }

    pub fn gauge_value(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.gauges
            .lock()
            .expect("metrics mutex poisoned")
            .get(&key(name, labels))
            .copied()
    }
}

impl MetricsRecorder for InMemoryMetrics {
    fn counter_inc(&self, name: &'static str, labels: &[(&str, &str)], value: u64) {
        *self
            .counters
            .lock()
            .expect("metrics mutex poisoned")
            .entry(key(name, labels))
            .or_insert(0) += value;
    }

    fn histogram_observe(&self, name: &'static str, labels: &[(&str, &str)], value: f64) {
        self.histograms
            .lock()
            .expect("metrics mutex poisoned")
            .entry(key(name, labels))
            .or_default()
            .push(value);
    }

    fn gauge_set(&self, name: &'static str, labels: &[(&str, &str)], value: f64) {
        self.gauges
            .lock()
            .expect("metrics mutex poisoned")
            .insert(key(name, labels), value);
    }
}

fn key(name: &str, labels: &[(&str, &str)]) -> String {
    let mut key = String::from(name);
    let mut labels = labels.to_vec();
    labels.sort_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
    for (label, value) in labels {
        key.push('|');
        key.push_str(label);
        key.push('=');
        key.push_str(value);
    }
    key
}
