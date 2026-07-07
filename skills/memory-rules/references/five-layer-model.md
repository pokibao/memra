# Three-Layer Cognitive Memory Model Reference

Based on cognitive science research on human memory systems.
Updated: 2026-03-01 (from v1.x five-layer to v3.0 three-layer)

## Layer Details

### L0: Identity Schema (Self-Concept)

**Cognitive Basis**: Core self-representation that remains stable across contexts.

**Contents**:
- Core project identity, immutable constraints
- User identity and preferences
- Brand rules, critical business logic

**Protection**:
- Requires 3 separate approvals to modify (`propose_change` + 3x `approve_change`)
- Cannot be written by AI directly
- Logged with full audit trail

**Persistence**: YAML + SQLite

### L2: Event Log (Episodic Memory)

**Cognitive Basis**: Tulving's episodic memory - "what, when, where" encoding.

**Contents**:
- Time-stamped events
- Location markers
- People involved
- Project milestones

**Features**:
- Optional TTL (time-to-live)
- Can be promoted to L3 after verification
- Searchable by time range, location, person

**Persistence**: SQLite, TTL optional

### L3: Verified Fact (Semantic Memory)

**Cognitive Basis**: General knowledge independent of personal experience.

**Contents**:
- Technical decisions and rationale
- Bug fixes and solutions
- Architecture patterns
- Validated facts

**Characteristics**:
- Permanent storage
- High confidence required (>= 0.9)
- Main layer for cross-session retrieval
- recall_count with Laplace smoothing (frequently recalled = higher quality)
- failure_count penalty (corrected memories get downranked)

**Persistence**: SQLite, permanent

## Removed Layers (v1.x → v3.0)

- ~~L1 Active Context~~: Was "working memory" — not persisted, session-only. Removed because it's inherently handled by the LLM's context window.
- ~~L4 Operational Knowledge~~: Was "skill schema" in `.ai/operations/`. Removed because operational knowledge lives in Skills/CLAUDE.md, not in the memory database.

## Layer Transitions

```
L2 (Event) → verification → L3 (Fact)
L3 (Fact) → propose_change() + 3x approve_change() → L0 (Identity)
```

## Retrieval Priority

1. L0 Identity - Always loaded at session start
2. L3 Verified Facts - Primary search target
3. L2 Event Log - For temporal queries
