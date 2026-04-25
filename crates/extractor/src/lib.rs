//! Article fetching + body extraction (readability / rule / summary fallback).
//! See docs/design/internal-dto-contracts.md § Extract stage.

// TODO Phase 1:
// - mod error;       // ExtractorError (ClassifiedError impl)
// - mod fetcher;     // HTML fetch with timeout + retry
// - mod readability; // primary strategy
// - mod rule;        // per-source rule-based strategy
// - mod fallback;    // summary-only fallback
// - mod content_hash;
