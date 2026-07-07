---
name: mem
description: Search and manage AI memory (Memra v6.0.1, SQLite + FTS5, no Qdrant)
---

# /mem — Memra quick command

Routes to Memra's 16 MCP tools. Backend is SQLite + FTS5 (Qdrant retired in v3+).

## Usage

```
/mem                       # Show memory status (uses ma CLI, not Qdrant)
/mem search <query>        # Hybrid semantic + lexical search
/mem add <content>         # Write a memory (cosine > 0.88 dedup)
/mem checkpoint <summary>  # Save task breakpoint
/mem context               # Wake-mode context snapshot
```

## Implementation

### No arguments — show status

```bash
# Use ma CLI (Rust binary at target/release/memra after `cargo build --release`)
~/projects/memra/target/release/memra doctor --project memra
```

### With arguments — call the right MCP tool

| Subcommand | MCP tool |
|---|---|
| `search <q>` | `mcp__memra__search_rules` (query=q) |
| `add <c>` | `mcp__memra__add_rule` (content=c, confidence=0.9) |
| `checkpoint <s>` | `mcp__memra__save_checkpoint` (summary=s, task_status=in_progress) |
| `context` | `mcp__memra__get_context` (mode=wake) |
| `find-cp [query]` | `mcp__memra__search_checkpoints` |

## Notes

- Requires the `memra` MCP server (from this plugin's `.mcp.json`).
- **No Qdrant**: v3+ moved to SQLite + FTS5 hybrid (semantic via FastEmbed/Ollama bge-m3 1024-dim + lexical via FTS5 + CJK bigram).
- Memory layers: L0 identity (3x approval) / L2 events (TTL optional) / L3 facts (permanent).
- Write dedup: cosine similarity > 0.88 returns `existing_id`; 0.70-0.88 auto-supersedes.
- Full 16-tool surface documented in `~/projects/memra/CLAUDE.md`.
