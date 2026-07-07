# scripts/ — Active R4 Inventory

R4 keeps this directory Rust-facing. Former v6 Python scripts, test helpers,
release gates, autoresearch runners, and launchd-era utilities were moved to
`_archive_python_v6/scripts/`.

## Active Entry Points

These are part of the current checkout:

- `local-ci.sh` — cargo-only gate plus active-shell syntax and Python-sunset checks. Run heavy modes on a Pro-class lab host, not on a lightweight laptop.
- `mcp_wrapper.sh` — MCP stdio wrapper; starts the Rust `target/release/memra serve` path and fails loud on stale archived-backend selection.
- `smoke.sh` — MCP smoke harness used after a release binary exists.
- `consolidate_nightly.sh` — Rust `memra consolidate ...` orchestration wrapper.
- `nightly-bench.sh` — Pro-class lab-host retrieval benchmark runner; writes 20-query judge results under `docs/dogfood-results/<timestamp>-bench/`.
- `ma-truth-banner.sh` — runtime database truth banner before MCP startup.
- `zombie-cleaner.sh` — optional launchd helper to terminate stale non-daemon `memra serve` processes older than 1 hour.
- `target/release/memra serve --daemon` — optional Pro-class always-on CLI forwarding daemon; warms embeddings once and serves `memra search` over `~/.memra/run/<project>.sock`.
- `ingest-roots.yaml.example` — example root list for `memra ingest --config ~/.memra/ingest-roots.yaml`.
- `check_no_plaintext_secrets.sh` — tracked-file secret scanner.
- `install-git-hooks.sh`, `git-hooks/pre-commit`, `git-hooks/pre-push` — local git hook installer/templates.
- `install_phase1_hooks.sh` — Rust hook installer support.
- `r3-real-gates.sh` — historical R3 real-gate runner retained for evidence replay; guarded to Pro-class lab hosts.
- `render_formula.sh`, `verify_homebrew_release.sh` — package/release support.
- `offsite-backup.sh`, `safe-merge.sh` — operator utilities.

## Data And Examples

- `launchd/*.plist.example` — examples only; install deliberately.
  - `com.memra.wiki-sync.plist.example` mirrors verified facts to `~/.memra/wiki/by-topic`.
  - `com.memra.morning-ingest.plist.example` runs static Markdown ingest at 07:00.
  - `com.memra.daemon.plist.example` keeps the local CLI forwarding daemon alive on Pro-class hosts.
  - `com.memra.nightly-bench.plist.example` runs the 20-query retrieval benchmark on Pro-class hosts.
  - `com.memra.zombie-cleaner.plist.example` trims stale `memra serve` siblings every 15 minutes.
- `parity_scenarios/*.jsonl` and `parity_scenarios/manifest.json` — legacy parity corpus retained as data for Rust tests and historical comparison.

## Archived Surfaces

Do not call archived scripts from this checkout. Use a separate `v6.3.0`
checkout if you need to inspect or reproduce v6 behavior:

- `_archive_python_v6/scripts/`
- `_archive_python_v6/autoresearch/`
- `_archive_python_v6/benchmarks/`
- `_archive_python_v6/backend/`
- `_archive_python_v6/tests/`

Last updated: 2026-05-17.
