//! Feed fetching + parsing + three-layer deduplication.
//! See docs/design/internal-dto-contracts.md § Feed stage.

// TODO Phase 1:
// - mod error;       // FeedError (ClassifiedError impl)
// - mod fetcher;     // reqwest + conditional GET (ETag / If-Modified-Since)
// - mod parser;      // feed-rs wrapper → FeedEntryMeta
// - mod normalize;   // link normalization + hash
// - mod dedup;       // three-layer: source+guid / link_hash / content_hash
