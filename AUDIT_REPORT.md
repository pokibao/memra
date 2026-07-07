# Memra Public Redaction Audit

Date: 2026-05-24
Status: ready for owner review, not pushed, not published

## Scope

This audit covers the nine-step local preparation lane from A.3.a through A.5 for turning the copied private source into a public Memra working tree.

The source tree started as an uncommitted import. To avoid placing private/raw material into git history, the first commit was made only after private personal code and obvious local noise were removed from the worktree. That means the early commits are path-grouped public-prep commits from an empty repository, not a raw-import baseline.

## Cleaned

- Removed the private personal layer from tracked source and replaced it with a public fallback module that preserves the exported API surface with empty entity lists.
- Renamed public-facing Rust packages, binaries, plugin metadata, Homebrew formulas, and launchd examples from Memory Anchor/MA naming toward Memra.
- Replaced private fixtures with synthetic data, including a 100-row synthetic topic-key fixture and regenerated embedding parity reference vectors.
- Redacted private names, companies, operational examples, Chinese business identifiers, amounts, and founder-specific literals from Rust tests, fixtures, docs, plugin metadata, scripts, and packaging notes.
- Added a public README with install, quick start, tool list, memory-layer summary, dream consolidation summary, and license/contribution notes.
- Added `.gitignore` entries for local build/runtime artifacts so `target/`, local env files, Qdrant API scratch files, cache dirs, `.bak`, and `.DS_Store` do not enter future commits.
- Removed the private prep goal file and the private `memra-core/src/personal` directory from the public worktree.

## Retained

- `LICENSE` retains the original licensor and copyright holder because this was explicitly required by the task. This is the only required-sensitive-list hit.
- The wider sanity grep still reports algorithmic learning-rule identifiers in code. These are not references to the similarly named company.
- Some compatibility names remain intentionally unresolved for owner review, including `/ma:` plugin command surfaces, `ma_*` internal adapter naming, `.ma-project`, `MA_*` environment variables, and `memory_anchor.sqlite3`. Renaming these is a migration/compatibility decision beyond pure redaction.

## Verification

Required sensitive-list grep:

Command shape: `rg -n -c "$SENSITIVE" .`, using the required sensitive list from the private prep checklist.

Result:

```text
LICENSE:2
```

Additional hidden sanity grep excluding `.git`, `.omx`, and `target` returned only `LICENSE` attribution plus algorithmic false positives.

Build and test verification:

```text
cargo build --release
Finished `release` profile [optimized] target(s) in 54.96s

cargo build --release --features personal
Finished `release` profile [optimized] target(s) in 56.55s

cargo test
All package, integration, binary, and doc-test suites passed.
```

Notable passing suites:

- `memra_core` library tests: 197 passed.
- `memra_server` library tests: 318 passed.
- `memra` binary tests: 318 passed.
- Redaction-sensitive targeted tests passed for commercial-number detection, recall/search fixture expectations, runtime process detection, and approver display names.
- Embedding parity fixture passed after regeneration.

## Commit Checklist

Completed local commits before this report:

- `[A.3.a] Strip private personal layer from public core`
- `[A.3.b] Replace topic key parity fixture with synthetic data`
- `[A.3.c] Redact Rust source comments and fixtures`
- `[A.3.d] Rename package metadata for Memra`
- `[A.3.e] Keep public license attribution`
- `[A.3.f] Add public Memra README`
- `[A.3.g] Ignore local build and runtime artifacts`
- `[A.4] Clean remaining public auxiliary surfaces`

This report is intended to be committed as `[A.5] Report public redaction audit`.

## Owner Review Items

- Confirm that retaining the current licensor attribution in `LICENSE` is still desired for public release.
- Confirm final GitHub repository and Homebrew tap URLs. Current public placeholders use `memra/memra`.
- Decide whether legacy compatibility namespaces (`/ma:`, `ma_*`, `.ma-project`, `MA_*`, `memory_anchor.sqlite3`) should remain for launch or be renamed in a separate migration-aware pass.
- Confirm MIT licensing is acceptable for the intended plugin/community marketplace.
- Review `README.md`, `.claude-plugin/plugin.json`, and `plugins/Memra/plugin.yaml` before publishing.

## Risk Level

Redaction status: green for the required sensitive grep, with license as the explicit retained exception.

Release-readiness status: yellow until owner confirms naming compatibility, repository URLs, license marketplace fit, and final README/plugin wording.

No push, release, or publishing action was performed.
