# T025: real Antigravity (agy) runs, 2026-11-26

Version: `agy --version` = 1.5.15. Model: gemini-4-hydrogen-medium (from `agy models`).

## Findings (probes run by hand, synthetic prompts)

1. `agy -p` takes the prompt as its value; it does not read stdin (`-p -` answered "How can I help you today?").
   `--input-format stream-json` reads stdin but needs `--output-format stream-json` and one NDJSON message per line:
   `{"event":"user","message":{"content":"..."}}`. The final answer is the `result` event
   (`status: SUCCESS`, `response`). A prompt given with `-p` is refused in that mode.
2. Contamination: by default agy loads the user's plugins (`coom`, `kanban-board`), their MCP servers and a
   rules file ("kanban-board Antigravity Rules (Caveman)"). `--disable-slash-commands` does not change that, and agy
   has no switch to ignore them. With `USERPROFILE` and `HOME` pointed at an empty directory the model listed only
   its 6 built-in skills (agy-customizations, antigravity-guide, conversations, run-workflow, swarm_monitor,
   swarm_pane), no plugins, no MCP servers, no rules; the saved sign-in still worked.
3. Quota: `agy` exits 3 with `RESOURCE_EXHAUSTED (code 429): Individual quota reached ... Resets in 3h28m26s`
   (the `result` event has `status: ERROR`); memb now recognises this (see quota.rs and the tests).

## Command (the shipped default)

```
agy --model gemini-4-hydrogen-medium --disable-slash-commands --input-format stream-json --output-format stream-json --print-timeout 150s
```
run by memb in `<data dir>\curator-sandbox` with `USERPROFILE`/`HOME` = `<data dir>\curator-home` and `MEMB_INTERNAL=1`.

## Real run through `memb curate` (opt-in test `real_agy`, shipped defaults: agy first, Codex fallback)

```
elapsed: 22.06s
stderr: memb: sending 1 chunk(s) of private conversation text to `agy, then codex` for summaries
memb: sending 1 evidence packet(s) with 1 chunk(s) (1467 bytes) of private conversation text to `agy, then codex`
stdout: {"auto_applied":[],"chunk_links":0,"chunks_sent":1,"dry_run":false,"embedded":0,...,"packets":1,"proposals":[1],"skipped":0,"staged_now":1,"summarized":1}
summary: "- User set a standing rule: from now on, always run the gate before tagging. ..." explicit_signal: Some(true)
test real_agy_summarizes_and_proposes_through_memb_curate ... ok
```
An earlier run (before the fallback was added) took 22.7 s with the same result.

## Real quota detection (Codex, quota exhausted until 2026-11-28 21:23 local)

```
memb: `codex` has used up its quota; not calling it for 58h57m
memb: curate failed: the model quota is used up; curation can resume in about 58h57m (`memb curate --retry-now` tries anyway)
```
