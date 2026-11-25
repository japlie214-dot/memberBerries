---
name: memb
description: Unified local knowledge graph memory (`topic` -> `knowledge` -> `claim`) with `memb.exe web` browser inspector.
---
# Memb Memory Skill (v0.4+)

- Memb stores all memories in **one unified local graph** (no `project` or `profile` parameter required).
- Organize domains naturally via `topic` hubs, attach durable concepts as `knowledge` notes linked to 1+ `topic`s, and record specific findings/rules as `claim` notes linked to 1+ `knowledge` notes.
- Store structured metadata (e.g., `confidence`, `source_chunks`, `repo`, `version`) in `properties`, keeping `desc` concise and readable.
- Prefer `update` for minor metadata edits, `supersede` when a new `claim` replaces an outdated one, and `retract` for soft-deletion with cascade safety.
- To browse the knowledge graph visually, run `memb.exe web` to open the read-only local browser inspector.
