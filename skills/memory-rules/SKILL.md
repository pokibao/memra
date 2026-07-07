---
name: Memra Rules
description: Use this skill when working with AI memory, searching for context, or saving important information. Provides the three-layer cognitive memory model and v6.0.1 usage guidelines (SQLite + FTS5 backend, no Qdrant).
version: 6.0.1
---

# Memra — AI's External Hippocampus

## Core Philosophy

Treat AI like an Alzheimer's patient — highly capable but prone to "forgetting" due to context compression. Memra is the external hippocampus that provides persistent memory across sessions.

## Three-Layer Cognitive Memory Model

| Layer | Code | Cognitive Equivalent | Persistence | Description |
|-------|------|---------------------|-------------|-------------|
| L0 | `identity_schema` | Self-concept | YAML + SQLite | Core identity, requires 3 approvals to modify |
| L2 | `event_log` | Episodic memory | SQLite, TTL optional | Events with time/place markers |
| L3 | `verified_fact` | Semantic memory | SQLite, permanent | Verified long-term facts |

## Mandatory Usage Rules

### 1. Session Start — Load Memory

```
mcp__memra__get_context(mode="wake")          # snapshot incl. identity + recent
mcp__memra__search_checkpoints(task_status="active")  # active task continuations
mcp__memra__search_rules(query="<topic>", start_time="<7d ago>")  # semantic fallback
```

### 2. Before Answering — Search First

MUST call `search_rules` (or `get_context` for broad recall) before answering questions about:

- "Previously...", "last time...", "history..."
- Design decisions, architecture choices
- Bug fix records
- Any context that is not "completely new"

### 3. After Task Completion — Write Memory

Trigger words: "done", "finished", "completed", "save progress"

Write flow:
1. Summarize key events using strong skeleton ([SUCCESS] / [FAILURE] / [CORRECTION])
2. Pick layer (most cases → `verified_fact`)
3. Call `add_rule` (built-in dedup: cosine > 0.88 returns existing_id; 0.70–0.88 auto-supersedes)

### 4. Task Checkpoints

```
mcp__memra__save_checkpoint(task_id, summary, task_status)
# task_status: in_progress | blocked | completed | abandoned
# UPSERT: same task_id auto-deactivates older checkpoints
```

ALWAYS close the loop with `task_status="completed"` — orphan active checkpoints
permanently pollute the wake-mode snapshot.

## 16-Tool Surface (v6.0.1)

**Core read/write (5)**: `search_rules`, `add_rule`, `get_context`, `propose_change`, `approve_change`

**Checkpoints (2)**: `save_checkpoint`, `search_checkpoints`

**Feedback (3)**: `report_outcome`, `report_artifact_feedback`, `report_topic_feedback`

**Experience (5)**: `search_experiences`, `review_experience`, `get_experience_surfaces`, `build_experience_artifacts`, `search_manifest`

**Auxiliary (1)**: `list_rooms`

## Observation JSON Template

```json
{
  "memory_kind": "<fact|event|procedure>",
  "category": "<decision|bug|routine|note|lesson|...>",
  "content": "<strong-skeleton text, see CLAUDE.md memory-protocol>",
  "confidence": 0.9
}
```

## Confidence Thresholds

| Confidence | Action |
|-----------|--------|
| >= 0.9 | Store directly to verified_fact |
| 0.7-0.9 | Pending approval |
| < 0.7 | Reject |

## Hybrid Search Backend (no Qdrant)

- Vector: FastEmbed / Ollama bge-m3 (1024-dim) — local, offline
- Lexical: SQLite FTS5 + CJK bigram tokenizer
- RRF fusion: relevance + freshness + trust three-axis weighted scoring
- Temporal markers (e.g. "最近", "今天"): freshness weight bumps to 40%
- Failure penalty: corrected memories downranked
- Supersede chain: deprecated x0.02, superseded x0.05

## Red Lines (Forbidden)

- AI CANNOT directly write to identity_schema (must use `propose_change` + 3x `approve_change`)
- CANNOT bypass 3-approval process for identity changes
- CANNOT answer history questions without searching memory
- CANNOT end important tasks without saving memory + closing the checkpoint
