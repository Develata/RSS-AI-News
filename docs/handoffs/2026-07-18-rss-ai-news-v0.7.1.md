# Handoff — v0.7.1 Rust acceptance matrix

## Metadata

- Date: 2026-07-18
- Owner: main
- Scope: `v0.7.0..v0.7.1` pre-tag candidate

## Done

- Kept `recent-entries --published-after` strictly opt-in: omitted input remains `None` and disables publication-time filtering.
- Confirmed the feature path stays cohesive: Clap parsing → command boundary → `RecentEntriesFlow` → dedicated read-only repository traits → shared SQLite/PostgreSQL query builder.
- Added the product-independent `rss-ai-news-acceptance` Rust CLI with `local`/`full` profiles, six independently runnable lanes, pretty/JSON reports, dry-run, fail-fast, bounded logs, redaction, and exact-name cleanup.
- Split acceptance code below the Deve-Notebook 500-line hand-written source fuse; maximum file size at audit time was 317 lines.
- Updated workspace/Docker wiring, README, architecture maps, operations docs, version identity, and release report.
- Closed independent-review findings: failure evidence redaction covers explicit and inherited sensitive env values; fail-fast prevents later smoke-resource creation.

## Validation

- `cargo test -p rss-ai-news-acceptance --locked`: 15 passed.
- `cargo clippy -p rss-ai-news-acceptance --all-targets --locked -- -D warnings`: passed.
- `cargo acceptance --format json run --profile local --expected-version 0.7.1`: 4 lanes / 27 steps passed.
  - Evidence: `/opt/data/tmp/rss-ai-news-v0.7.1-local-final.json`
  - Evidence SHA-256: `fbb1f316d9fd78aac70a3738c5a41ef81cad03b46730d378370ae8a9637ac2cf`
- `cargo acceptance --format json run --profile full --expected-version 0.7.1 --dry-run`: 6 lanes / 47 planned steps; smoke workspace delta 0.
  - Evidence SHA-256: `c952159c21baf5d074f8449e059aaebab897cdb3cad64995629295cc09f8486a`
- Release binary: `rss-ai-news 0.7.1`.
  - SHA-256: `e670ccf6d65f2677210c64700bb8468c06de439e1ecdcb68abf97669edb6eeef`
- Independent Codex review: product cutoff/coupling PASS; fail-fast and explicit/inherited credential redaction findings closed; final closure PASS. Procedural blockers remain commit/remote evidence sequencing only.

## Blocked / Pending

- Commit the complete candidate and rerun exact-commit focused gates.
- Push candidate commit and wait for exact-SHA GitHub CI, including PostgreSQL and Docker lanes.
- Replace pending remote evidence in the release report with real run URLs/results before tagging.
- Push annotated `v0.7.1`, wait for the tag-triggered release workflow, and read back remote tag/image manifests.

## Next entry point

1. Review `git diff` and stage exact paths only.
2. Commit the v0.7.1 candidate.
3. Run `cargo acceptance run --profile local --expected-version 0.7.1` against the exact commit.
4. Push `main`; gate on exact commit SHA CI.
5. Fill `docs/reports/releases/v0.7.1.md`, commit/push the evidence snapshot, gate that docs commit, then tag.
