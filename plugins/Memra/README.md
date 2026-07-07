# Memra Claude Code Plugin

Memra connects Claude Code to a local persistent memory runtime. The plugin injects compact context at session start, refreshes memory before compression, and can save durable checkpoints and governed facts through MCP tools.

## Install

```text
/plugin install Memra
```

## Tools

| Plugin tool | Memra MCP tool |
| --- | --- |
| `search_rules` | `search_rules` |
| `add_rule` | `add_rule` |
| `get_context` | `get_context` |
| `propose_change` | `propose_change` |
| `approve_change` | `approve_change` |
| `save_checkpoint` | `save_checkpoint` |
| `search_checkpoints` | `search_checkpoints` |
| `report_outcome` | `report_outcome` |

## Runtime

The plugin uses the Rust `memra` binary through the repository wrapper scripts. If the runtime is unavailable, hooks should degrade without interrupting the host agent.
