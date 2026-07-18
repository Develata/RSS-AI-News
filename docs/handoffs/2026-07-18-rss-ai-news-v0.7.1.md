# Handoff — v0.7.1 Rust acceptance matrix

## Metadata

- Date: 2026-07-18
- Owner: main
- Scope: released and fully verified `v0.7.1`

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
- Independent Codex review: product cutoff/coupling PASS; fail-fast and explicit/inherited credential redaction findings closed; final closure PASS.
- GitHub CI exact head SHA `c26ac97afddb1624f7b27eef9fb284fb33c18a65`: [run 29650615837](https://github.com/Develata/RSS-AI-News/actions/runs/29650615837), 5/5 jobs PASS（fmt/clippy、workspace、SQLite migration、PostgreSQL、Docker）。
- Evidence/tag commit `3f13e956763c2a41c86df6e0d609a39b2418c0ef`: [CI run 29650877546](https://github.com/Develata/RSS-AI-News/actions/runs/29650877546), 5/5 jobs PASS。
- Remote annotated tag object `76596b12d667c0b841a9f73b5e735ffbaa0234d4` peels to `3f13e956763c2a41c86df6e0d609a39b2418c0ef`。
- Tag [release run 29651042474](https://github.com/Develata/RSS-AI-News/actions/runs/29651042474): runtime + scheduler GHCR push PASS。
- Runtime OCI index: `sha256:eda3ead30fc9788bf7da1ef238c997b5e562dd645fe429a561c86e36fc220eb7`；scheduler index: `sha256:46a391303e229af542ebf85bf21759e9a2f1108f7324c7f839c33490fa272683`；semver/minor/latest alias assertions PASS。
- Authorized remote binary execution smoke: runtime child manifest `sha256:4a6bcd0d3fedb0b773d66559fb1900ad2708c01b4eda491818b4d14373638d29` and scheduler child manifest `sha256:01ec24be204a1831058bb1c16911233b56f17137fd6485f2ea1a56b8501fd1e5` were fetched from GHCR with every consumed digest verified. Both embedded binaries have SHA-256 `6c15b9fb593f268425efe39228c435a5fffebce743d088e4a568ca324b68ecbc`; both `--version` and `--help` exited `0` under a minimal non-secret environment. Evidence: `/opt/data/tmp/rss-ai-news-v0.7.1-remote-smoke.json`, SHA-256 `18e1df17501922b45da6fdbbb7f69ffe57bcae5b4cf3a1039d00fc0142bfc7d4`。

## Blocked / Pending

- None.

## Next entry point

No v0.7.1 follow-up is required.
