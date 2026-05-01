//! Tracing subscriber, redaction, metrics facade, doctor health primitives.

pub mod health;
pub mod metrics;
pub mod redact;
pub mod tracing_init;

pub use health::{CheckOutcome, CheckReport, HealthCheck};
pub use metrics::{InMemoryMetrics, MetricsRecorder, NullMetrics};
