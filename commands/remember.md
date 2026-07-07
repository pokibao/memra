---
name: remember
description: Quick add memory with automatic layer detection (Memra v6.0.1, SQLite + FTS5)
---

# /remember — Quick add memory

Save something to Memra without specifying layer. Plugin layer auto-detects.

## Usage

```
/remember <what to remember>
```

## Examples

```
/remember Fixed search_rules returning empty list when query had only stop-words; root cause: FTS5 tokenizer dropped CJK bigrams below threshold
/remember Decided to use Rust backend (target/release/memra serve) as default in v6.0.1; Python remains opt-in via MA_BACKEND=python
/remember User prefers Chinese responses for tech-explanation tasks
/remember [SUCCESS] MBP fresh deploy plugin via mise + rsync target binary, 30min, no brew/sudo
```

## Implementation

1. Analyze content to pick layer:
   - Contains "decided", "chose", "architecture" → `memory_kind=fact` (L3 verified_fact)
   - Contains "fixed", "bug", "resolved" → `memory_kind=fact` (L3)
   - Contains "today", "just now", "meeting" → `memory_kind=event` (L2 event_log)
   - Default → `memory_kind=fact` (L3)

2. Pick category:
   - Contains person names → `category=person`
   - Contains place words → `category=place`
   - Contains "bug", "fix", "feature" → `category=event`
   - Contains "[SUCCESS]" / "[FAILURE]" / "[CORRECTION]" prefix → `category=lesson` (or routine for SUCCESS)
   - Default → `category=event`

3. Call `mcp__memra__add_rule` with:
   - `content`: user input (verbatim)
   - `memory_kind`: detected layer
   - `category`: detected category
   - `confidence`: 0.9 (high — user explicitly asked to remember)

4. Confirm to user with the saved memory ID + layer + any auto-supersede notice.

## Notes

- Built-in dedup: if cosine similarity > 0.88 with existing, returns `existing_id` instead of creating a new memory.
- Auto-supersede: 0.70–0.88 marks the older version superseded automatically.
- For long-term decisions / lessons, prefer the strong-skeleton format from `~/.claude/CLAUDE.md` memory-protocol section (`[CORRECTION]` / `[SUCCESS]` / `[FAILURE]` with trigger / 现象 / 根因 / 规避 / 路由 fields).
