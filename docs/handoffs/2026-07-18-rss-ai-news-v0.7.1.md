# Handoff — v0.7.1 Rust acceptance matrix

## Metadata

- Date: 2026-07-18
- Owner: main
- Scope: `v0.7.0..v0.7.1` tag-ready candidate

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
- Exact commit `c26ac97afddb1624f7b27eef9fb284fb33c18a65`：`cargo acceptance --format json run --profile local --expected-version 0.7.1`，4 lanes / 27 steps passed.
  - Evidence: `/opt/data/tmp/rss-ai-news-v0.7.1-commit-c26ac97-local.json`
  - Evidence SHA-256: `55be116238a31001a5005043fe38c225b10bfafbc73989f2d6452fbe60eb31c0`
- Exact commit：`cargo acceptance --format json run --profile full --expected-version 0.7.1 --dry-run`，6 lanes / 47 planned steps；smoke workspace delta 0。
  - Evidence SHA-256: `fddfdb9fa4bfada2d1ae8c4020a91c5cf5c00155911bb61967e19c06670c8250`
- Release binary: `rss-ai-news 0.7.1`.
  - SHA-256: `e670ccf6d65f2677210c64700bb8468c06de439e1ecdcb68abf97669edb6eeef`
- Independent Codex review: product cutoff/coupling PASS; fail-fast and explicit/inherited credential redaction findings closed; final closure PASS. Procedural blockers remain commit/remote evidence sequencing only.
- GitHub CI exact head SHA `c26ac97afddb1624f7b27eef9fb284fb33c18a65`: [run 29650615837](https://github.com/Develata/RSS-AI-News/actions/runs/29650615837), 5/5 jobs PASS（fmt/clippy、workspace、SQLite migration、PostgreSQL、Docker）。

## Blocked / Pending

- Commit/push this docs-only evidence snapshot and gate its exact SHA.
- Push annotated `v0.7.1`, wait for the tag-triggered release workflow, and read back remote tag/image manifests.

## Next entry point

1. Exact-path stage and commit the release report + handoff evidence update.
2. Push `main`; gate on the evidence commit SHA CI.
3. Create/push annotated `v0.7.1` at that exact commit.
4. Gate the tag workflow and read back remote tag + runtime/scheduler image manifests.
