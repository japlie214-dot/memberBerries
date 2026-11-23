# Phase coverage and retirement gate

This is a requirements ledger, not a release claim. **Implemented** means the local v0.1 implementation exists; native host verification is separate. **Replaced** means the design intention is addressed differently. **Open** means the phase software and notes must be retained until implementation or an approved disposition.

| Source | Requirement | v0.1 disposition |
|---|---|---|
| Shared design / 1 | Windows-first Rust daemon and CLI, one project memory across clients | **Replaced:** one binary, stdio per client, one OS-serialized redb store; no background process or HTTP service. |
| Shared design / 1 | 2 GiB Win32 Job Object and kill-on-close; 1200/1550/1750 MiB governor thresholds and bounded redb cache | **Partial:** 16 MiB configured redb cache, bounded note/query/frame input; hard cap, process-tree ownership, governor and budget tests **open**. |
| 1 | Topic/knowledge/claim hierarchy and durable graph | **Implemented:** explicit project, typed parent links, atomic updates, monotonic IDs. **Replaced:** restrict deletion of parents instead of implicit cascades; migration and force cascades remain open. |
| 1 | User/assistant-only scroll; 90 messages → keep 10; immutable chunks | **Open:** no automatic capture or raw scroll collection. Future capture needs opt-in, content checks before persistence and tests against loss/duplication at rollover. |
| 1 | Latest user message only; skip under 100 characters and slash commands; dedup resets every seven chunks | **Open:** automatic retrieval/injection not enabled. Explicit search has its own nonempty/512-byte limits. |
| 2 | Managed `llama-server`, CPU ISA selection, EmbeddingGemma prefixes, Matryoshka dimensions, idle unload | **Open:** no model or sidecar is shipped. Confirm model availability, redistribution, process lifecycle, and validity of VRAM/RAM assumptions first. |
| 2 | Query LRU (256 entries), document embedding cache, quantized project working set and version invalidation | **Open** pending embeddings; v0.1 bounded deterministic lexical scan needs no model caches. |
| 2 | LanceDB vector ANN + BM25/FTS + hybrid RRF | **Open:** v0.1 uses lexical token overlap; LanceDB is not linked. Keep redb authoritative and any future vector index rebuildable. |
| 2 | One-hop knowledge/claim neighbors with no topic fan-out | **Implemented:** bounded, non-recursive expansion, covered by subprocess tests. |
| 3 | Layer 1 summarizes and embeds immutable chunks; Layer 2 runs after sweep or at three uncurated chunks | **Open:** no model backend or automatic graph writes. |
| 3 | Evidence thresholds: chunk corroboration at 0.72, existing-node match at 0.70, or explicit directive; 30-day staging TTL | **Open:** detailed Phase 3 uses OR between these routes; the summary's AND shorthand conflicts. Retain OR as the proposed acceptance rule. |
| 3 | Source chunk pointers, raw drill-down, staging and curator validation | **Open:** current notes store only user-approved text; no raw transcript read tool. Future provenance needs access controls and a deletion lifecycle. |
| 3 | Per-type CRUD, claim supersede/retract, preservation of multi-parent graph integrity | **Partial:** remember/search/inspect/forget enforce typing and referential integrity. Mutation history, protected reparenting, supersede/retract, and revision-scoped edge updates **open**. |
| 3 | Direct model APIs and headless CLI backends with recursion guard | **Open:** no providers, scripts, credentials or cloud transmission in the runtime. Backend use and user consent must be verified independently. |
| Shared design / 3 | Three lifecycle events only; `MEMD_NO_MEMORY` and ignore directories; no tool/compaction/end-session logs | **Partial:** no automatic ingestion at all. Opt-in hooks and deterministic exclusion before storage are **open**. Host events are not interchangeable: Codex uses typed hooks; AGY uses `PreInvocation` and `Stop` and may invoke before each model call. |
| 4 | Routine at 15:45 WIB, 20-hour cooldown, ≥24-hour catch-up; sweep idle >24h and ≥45 messages, keep 10 | **Open:** no scheduled or background tasks; future timing tests must cover restarts, wake and failed runs without marking failure complete. |
| 4 | Consolidation, incremental compaction, history, graph-consistent rollback and Markdown mirror | **Open:** no scheduled export or data compaction; deleting a note is not an erasure guarantee. |
| 4 | Optional Python sidecar, zero idle RAM; Windows logon task, not a system service | **Open:** no sidecar or task, and no persistent runtime. Future worker lifecycle must be tested rather than inferred from a standalone script. |
| Release | Cowork `.plugin`, Desktop `.mcpb`, native AGY and Codex companions | **Built:** shared Windows executable and packager; native host support must be checked independently. See [verification matrix](Install.md#verification-matrix). |

## Retirement checklist

1. Verify each intended host with synthetic save → cross-host recall → restart → forget → uninstall. Verify that an import actually makes tools callable. Record host version and package hash.
2. Turn every open requirement into tested behavior or obtain the user's explicit approval for a documented replacement or omission. Retain enough detail to reproduce exact phase behavior without the original files.
3. Have Kanban QA review release evidence and this ledger. Do not pass cleanup on archive shape, unit tests, or partial coverage alone.
4. Only then delete the four specifically named `memd-phase1-windows` through `memd-phase4-windows` directories. Confirm exact paths and keep any unrelated files. Do not clean the entire working directory.
