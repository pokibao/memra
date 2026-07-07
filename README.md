# Memra

Persistent AI memory infrastructure for Claude Code.

Memra helps coding agents carry durable context across sessions, compactions, and long-running projects. It is built around a local Rust MCP runtime, SQLite storage, FTS5 retrieval, optional embeddings, and a governed write path so important facts can be recalled without turning every new session into a manual context rebuild.

## Why

Claude Code users often lose useful project state when a session ends or context is compacted. The pain is visible in long-running community threads such as `anthropics/claude-code#6235`: users want agents that remember decisions, checkpoints, and corrections without repeatedly pasting the same background.

Memra focuses on that exact gap:

- durable project memory across sessions
- explicit checkpoints for resuming work
- governed writes for corrections and verified facts
- local-first storage with a Rust MCP server
- graceful fallback when embeddings or background services are unavailable

## Install

```text
/plugin install Memra
```

For local development:

```bash
cargo build --release
./target/release/memra doctor --project memra-demo
```

## Quick Start

```bash
memra init --project memra-demo
memra add-rule "Prefer regression tests before refactors" --project memra-demo
memra search "refactor testing preference" --project memra-demo
memra save-checkpoint "Public release prep is ready for review" --project memra-demo
```

## Memory Layers

Memra keeps memory in explicit layers:

- `L0 identity`: stable user or project principles that should rarely change
- `L2 event`: time-bound events, outcomes, and session history
- `L3 verified_fact`: durable facts that have been confirmed or approved

This structure lets the agent distinguish identity, recent activity, and verified knowledge instead of flattening everything into one untrusted note stream.

## MCP Tools

The public Claude Code plugin surface centers on eight tools:

| Tool | Purpose |
| --- | --- |
| `search_rules` | Search durable rules and facts |
| `add_rule` | Add a governed memory entry |
| `get_context` | Build a compact context snapshot |
| `propose_change` | Propose a change to governed memory |
| `approve_change` | Approve a proposed governed-memory update |
| `save_checkpoint` | Save a resumable task checkpoint |
| `search_checkpoints` | Find prior checkpoints |
| `report_outcome` | Attach outcome feedback to memory |

## Dream Consolidation

Memra includes a consolidation path for turning raw activity into higher-signal memory. The goal is not to store every message. It is to promote useful patterns, corrections, and project facts into durable entries while keeping noisy or unconfirmed material out of the main recall path.

## License

Memra is licensed under MIT. See [LICENSE](LICENSE).

## Contributing

Contributions should keep the public package free of private data, project-specific customer references, and local operator workflows. Before opening a release or marketplace submission, run the full build, test, and redaction audit described in `AUDIT_REPORT.md`.
