//! Flow coordination: orchestrates ability crates (feed, extractor, ai, report,
//! publish) through state-machine transitions.
//!
//! Owns the run lifecycle: run_id allocation, lease management, retry policy,
//! and run_events emission.

// TODO Phase 1:
// - mod error;          // RuntimeError (ClassifiedError impl)
// - mod run;            // Run / RunContext (run_id, started_at, config snapshot)
// - mod flows {
//       ingest,         // feed → dedup → extract → persist
//       ai_run,         // claim articles → AI → persist result
//       publish,        // select → snapshot → render → publish
//   }
// - mod replay;         // artifact replay driver
// - mod backfill;       // backfill driver
// - mod scheduler;      // (later) periodic run trigger
