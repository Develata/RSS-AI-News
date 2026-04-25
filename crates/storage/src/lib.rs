//! Persistence layer: SQLite-first with PostgreSQL as replacement target.
//!
//! Owns: migrations, repositories, lease/claim primitives, raw_artifacts store.

// TODO Phase 1:
// - mod error;              // StorageError (ClassifiedError impl)
// - mod pool;               // sqlx connection pool builder
// - mod migrations;         // embed sqlx::migrate!()
// - mod repo {              // repositories per object
//       feed_source, feed_entry, article, article_ai_result,
//       publish_record, raw_artifact, run_event, rule_version,
//   }
// - mod lease;              // claim/lease/reclaim helpers
