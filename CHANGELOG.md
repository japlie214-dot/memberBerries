# Changelog

One iteration is one version. Before a new package is built, the number goes up
(`python scripts/bump-version.py minor|patch`, which refuses to run until this file has an
entry for the new version). While the version is below 1.0.0: **minor** = an iteration that
adds or changes behaviour, **patch** = an iteration of fixes only. 1.0.0 is the release gate (T014). Commits that change only evidence or wiki text and ship in no package do not need their own number.

## 0.8.0 (2026-11-26)

- **Retrieval caches:** the knowledge base has a version; the i8 hot working set is kept in memory and persisted with the version it was built at, so a search with an unchanged base reads and embeds nothing. Document vectors use a stable SHA-256 content key (model, truncation, title, description) instead of the note id plus a `DefaultHasher` fingerprint; unchanged notes are never embedded again and identical text shares a vector. Old cache tables are dropped.
- **One-hop expansion** uses an adjacency index and point reads (no load of all notes) and adds at most 4 claims per knowledge note.
- **Prompt formats:** the loopback server path now sends `task: search result | query: ...` and `title: ... | text: ...`; the in-process path puts the real title in the title slot. Vectors are re-embedded once after upgrading. Query truncation stays at 128 tokens (about 512 characters on the server path).
- The loopback server path without `MEMB_EMBED_MODEL_ID` is lexical only (it used to embed every note on every query).
- `search` no longer scans the notes just to validate the query.

## 0.7.1 (2026-11-26)

- Consolidation compares at most the newest 2000 knowledge and claim notes per run, so a large store cannot make the routine slow.

## 0.7.0 (2026-11-26)

- **Daily routine (Phase 4 maintenance):** thin scrolls (under 45 messages) idle for 14 days are pruned; near-duplicate knowledge or claim notes become reversible pending merge proposals (update the survivor, retract the duplicate; never applied by the daemon); `memory.redb` is compacted at the end of each run. `memb routine run-now` reports `pruned_scrolls`, `merges_proposed` and the database size before and after.
- **Mirror:** files now carry `source_chunks` in the frontmatter and `## Children` wiki-links next to `## Parents`.

## 0.6.0 (2026-11-26)

- **Diagnostics:** new `memb doctor`, `memb inspect session <id>`, `memb chunk read <chunk id>` (gated like the `scroll_chunk_read` tool), `memb routine run-now` (skips the schedule, not the lease) and `memb history` (the Phase 4 name for `memb revisions`, which still works). Output never contains note or chat text, except `chunk read`.

## 0.5.0 (2026-11-26)

- **Curation models:** the default headless model is Antigravity `agy` (gemini-4-hydrogen-medium, stream protocol, run with an empty home so none of your Antigravity plugins, MCP servers or rules load); Codex (gpt-6-luna, medium, isolated from your Codex plugins, skills, hooks, memories and AGENTS.md) is the fallback. New `curator.fallback_command`; `curator.cli_protocol` was removed again (an `agy` program is detected by name).
- **Quota handling:** a model whose usage quota is used up is not called again until the reset time it names (default wait 15 min, doubling to 6 h); the other model answers meanwhile. `memb curate --retry-now` forgets all waits.
- **Process control:** the model command starts suspended and joins a kill-on-close Job Object before it runs; the daemon and MCP server share a job-wide 2 GiB cap with kill-on-close; a memory governor drops the idle embedding model (300/600 s by hardware tier) and, from 1550 MiB, the model and the query cache; the daemon pauses background work at 1750 MiB.
- **Hooks:** slash commands never trigger retrieval; a note updated after it was injected is injected once more; Antigravity capture fixed for agy 1.5.15 (`transcript_full.jsonl`, wrapped user text) and verified in a live session; Claude Desktop capture verified live.
- **Auto-apply, curator links and packets, rollback** (opt-in) from 0.4.x work are unchanged in behaviour.
- Fixes: `explicit_signal: null` is read as "no"; the daemon starts even if the job limit cannot be applied.

## 0.4.0 (2026-11-25)

- Baseline of the package set (Cowork `.plugin`, Codex and Antigravity companions) with the MCP server, web inspector, opt-in capture hooks, daemon, `memb curate` and `memb proposals`.
