# Changelog

One iteration is one version. Before a new package is built, the number goes up
(`python scripts/bump-version.py minor|patch`, which refuses to run until this file has an
entry for the new version). While the version is below 1.0.0: **minor** = an iteration that
adds or changes behaviour, **patch** = an iteration of fixes only. 1.0.0 is the release gate (T014).

## 0.5.0 (2026-11-26)

- **Curation models:** the default headless model is Antigravity `agy` (gemini-4-hydrogen-medium, stream protocol, run with an empty home so none of your Antigravity plugins, MCP servers or rules load); Codex (gpt-6-luna, medium, isolated from your Codex plugins, skills, hooks, memories and AGENTS.md) is the fallback. New `curator.fallback_command`; `curator.cli_protocol` was removed again (an `agy` program is detected by name).
- **Quota handling:** a model whose usage quota is used up is not called again until the reset time it names (default wait 15 min, doubling to 6 h); the other model answers meanwhile. `memb curate --retry-now` forgets all waits.
- **Process control:** the model command starts suspended and joins a kill-on-close Job Object before it runs; the daemon and MCP server share a job-wide 2 GiB cap with kill-on-close; a memory governor drops the idle embedding model (300/600 s by hardware tier) and, from 1550 MiB, the model and the query cache; the daemon pauses background work at 1750 MiB.
- **Hooks:** slash commands never trigger retrieval; a note updated after it was injected is injected once more; Antigravity capture fixed for agy 1.5.15 (`transcript_full.jsonl`, wrapped user text) and verified in a live session; Claude Desktop capture verified live.
- **Auto-apply, curator links and packets, rollback** (opt-in) from 0.4.x work are unchanged in behaviour.
- Fixes: `explicit_signal: null` is read as "no"; the daemon starts even if the job limit cannot be applied.

## 0.4.0 (2026-11-25)

- Baseline of the package set (Cowork `.plugin`, Codex and Antigravity companions) with the MCP server, web inspector, opt-in capture hooks, daemon, `memb curate` and `memb proposals`.
