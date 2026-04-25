//! Tracing subscriber, run_events writer, metrics, doctor health helpers.
//! See docs/design/error-and-observability.md.

// TODO Phase 1:
// - mod tracing_init;  // subscriber setup (env filter, json formatter)
// - mod run_events;    // persistent run_events record builder
// - mod metrics;       // counter/histogram façade
// - mod doctor;        // health-check primitives
