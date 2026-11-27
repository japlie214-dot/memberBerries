# Changelog

One iteration is one version. Before a new package is built, the number goes up
(`python scripts/bump-version.py minor|patch`, which refuses to run until this file has an
entry for the new version). While the version is below 1.0.0: **minor** = an iteration that
adds or changes behaviour, **patch** = an iteration of fixes only. 1.0.0 is the release gate (T014). Commits that change only evidence or wiki text and ship in no package do not need their own number.

## 0.15.0 (2026-11-27)

- **Change (T032):** what a hook injects into a prompt is bounded: at most `retrieval.max_inject_tokens` (750 tokens, counted as 4 bytes each: 3000 bytes) of context, best match first; a note that does not fit is skipped and not marked as injected, so a later prompt can still bring it. Before, a hook could inject about 34 KB (8 notes of about 4 KB). New `[retrieval]` settings `max_inject_tokens`, `top_k` (5) and `score_floor`. **The vector score floor default is now 0.60 (was 0.25), chosen by measurement on the local EmbeddingGemma 2 q4 model only** (unrelated notes score about 0.6 there, so 0.25 filtered nothing; right notes scored 0.617 to 0.839). The floor applies to every embedding backend and Snowflake and loopback-server scores were not measured: if their right-note scores sit below 0.60 their vector hits are dropped and only lexical hits remain; set `retrieval.score_floor` to adjust. Evidence: `evidence/t032-injection-floor.md`.

## 0.14.0 (2026-11-27)

- **Change (T036):** a search never waits for the vector index. When the index is missing or out of date, a search answers at once from lexical search plus the vectors already saved (0.5 s for 100 notes with the real model, where building the index inside the search took 33 s), and the MCP server embeds the missing notes in a background thread, 16 at a time, saving each chunk so an interrupted build resumes without embedding finished notes again. Hooks and `memb search` never start a build. `memb doctor` has a new `vector index` line (up to date, unfinished with the number of saved vectors, or not built yet). Evidence: `evidence/t036-hot-set-background.md`.

## 0.13.3 (2026-11-27)

- **Test (T044):** the hook deadline is now proven against a real pipe service whose handler takes 3 s: a hook (kick mode) gives up at 2 s, the rest of that hook skips the service at once, and a search (wait mode) still gets the answer. The same build was timed live with the model unloaded after idle (kick-mode search 2.5 s, 2.5 s, then 0.7 s) and with `memb hook` against a freshly started service (2.5 s while the model loads, then 0.8 s). Corrects the T043 timings, which came from runs that never reached the service. No change in behaviour.

## 0.13.2 (2026-11-27)

- **Fix (T043):** a hook no longer waits for the embedding model to reload. When the shared service is running but has unloaded its model after 5 idle minutes, loading it takes longer than the 10 s hook timeout; hooks now give up after 2 s (and skip the service for the rest of that hook), answer lexically, and the model is warm for the next request. Searches from the MCP server and CLI still wait.

## 0.13.1 (2026-11-27)

- **Fix (T042):** the shared embedding service no longer keeps the caller waiting. It was started with the caller's inherited handles, so anything that captured memb's output (a host, a shell, a hook runner) saw no end-of-output until the service exited 15 minutes later. It is now started by `memb_win::spawn::spawn_detached`, which passes no handles at all.

## 0.13.0 (2026-11-26)

- **Shared embedding service (T042):** with a local model, every memb process (each host's MCP server, every hook) now asks one resident `memb embed-service` for vectors over a per-user named pipe (`\.\pipe\memb-embed-v1-<hash>`, current-user-only access list, remote clients refused, no TCP port). One model in memory instead of one per host; hooks no longer load it. The service starts on demand (hooks only start it for next time and answer lexically meanwhile), unloads the model after 5 idle minutes and exits after 15 (`MEMB_EMBED_SERVICE_IDLE_SECS`). `memb embed-service status|stop`; `MEMB_EMBED_SERVICE=0` keeps the model in-process.
- The host job object now allows breakaway so the service outlives the process that started it.

## 0.12.0 (2026-11-26)

- **Snowflake AI_EMBED backend:** `[embeddings.snowflake]` (account, user, warehouse, key file, optional role and model). memb signs in with an unencrypted PKCS#8 key (RS256 JWT, the key never leaves the file), calls the SQL API through Windows' curl.exe, embeds notes 16 per statement, handles partitioned results and tags vectors with the model and prompt policy. The default model `snowflake-arctic-embed-m-v1.5` gets the retrieval prefix on queries; `snowflake-arctic-embed-l-v2.0` gets none (measured). Used only when configured and when no local model is chosen or installed. `memb doctor` reports it and checks the key file without any request.
- `embed_note` gives every backend its own document format (used when linking chunks to notes).
- Live measurements on this project's synthetic corpus: `evidence/t031-snowflake-ai-embed.md`.
- Departure ledger: rows 18 (shared embedding service, T042, approved) and 19 (Snowflake) added.

## 0.11.0 (2026-11-26)

- `memb models disable` and `memb models enable`: stop or resume automatic use of an installed model without deleting it. `memb models install` re-enables; `remove` deletes. Status and doctor say when it is disabled.
- `memb doctor` now reports an installed model (it said lexical-only while one was installed, because it does not load the environment the way the server does).
- `MEMB_EMBED_THREADS` sets the ONNX Runtime threads per embedding (default 2).
- An opt-in benchmark (`crates/memb/tests/embed_bench.rs`) and the recorded 4-bit results on a 12-core, 16 GB, AVX2 machine (`evidence/t036-embeddinggemma-q4-benchmark.md`).

## 0.10.0 (2026-11-26)

- **Hardware-aware model build (Phase 2 tiers):** `memb models install` picks the EmbeddingGemma 2 build that fits the machine: 4-bit with 256-wide vectors on a weaker machine, 8-bit with 512-wide vectors with AVX2 or AVX-512, 8 physical cores and 16 GB. A machine below the floor (no AVX2 with FMA or AVX-512, or under 8 GB RAM) is told so; `--variant q4|int8` overrides. memb uses the installed build and its paired width automatically (a width you set in `embeddings.mrl_dim` wins). `memb models status` and `memb doctor` show the machine and the recommendation. The in-process loader accepts either build.
- **Curator prompt:** adds the Phase 3 rubric: defer what will not matter in two weeks, do not repeat numbers a claim already carries, prefer a claim over a longer description.

## 0.9.0 (2026-11-26)

- **EmbeddingGemma 2 install pipeline:** `memb models install | status [--verify] | remove`. One consent step, a pinned Hugging Face revision, size and SHA-256 verification, resume after interruption, files visible only once verified. When the model is installed and no other backend is chosen, memb uses it with no configuration. `embeddings.auto_download = true` lets the daily routine fetch it. `memb doctor` reports the model and points to the command.
- **Query budget is 512 tokens** (was 128) on the in-process path, and about 2,048 characters on the loopback path. The model contract changed, so vectors are re-embedded once.
- Departure ledger (T024) recorded in `wiki/Phase-Coverage.md`: all approved except the per-query version check, which becomes T029.

## 0.8.1 (2026-11-26)

- Retrieval: a note retracted after the hot set was built no longer takes one of the five search seeds (the vector candidates are checked against the live notes with point reads).
- Tests: the "no model identity" check now proves no connection is made to the embedding server, with a control that does connect.

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
