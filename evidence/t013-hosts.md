# T013: real-host hook smoke (in progress), 2026-11-26

## Antigravity (agy 1.5.15): VERIFIED live

Method: isolated home (`USERPROFILE`/`HOME` = `C:\tmp\t013\agy\home`, so the user's own Antigravity
config is untouched), `agy plugin install` of the unzipped `memb-agy.zip`, isolated data dir
(`MEMB_DATA_DIR`), `MEMB_SCROLL_CAPTURE=1`, synthetic prompt through `agy -p`.

Real findings (assumptions that failed on the first live run, both fixed in `crates/memb/src/main.rs`):

1. `Stop` hook ran (`command_hook_executor`: "memb: hook skipped; invalid input ..."): the hook found
   no transcript. agy 1.5.15 writes `.../brain/<id>/.system_generated/logs/transcript_full.jsonl`, not
   `transcript.jsonl`. Both names are now accepted (any other name is still refused; test added).
2. The user record is wrapped: `<USER_REQUEST>\n<text>\n</USER_REQUEST>\n<ADDITIONAL_METADATA>...`. memb now
   stores only the text between the tags.

Real `Stop` payload (keys): artifactDirectoryPath, continuationActive, conversationId, cwd, error,
executionNum, fullyIdle, hookEventName ("Stop"), lastUserInput, modelName, terminationReason
("NO_TOOL_CALL"), transcriptPath, workspacePaths. The hook ran with `server\memb.exe` resolved from the
plugin directory (R4 for agy: confirmed; `hooks.json` uses a relative path and no `${PLUGIN_ROOT}`).

After the fix (release build, repackaged, plugin reinstalled in the isolated home):

| Run | Result read back from the data dir |
|---|---|
| `MEMB_SCROLL_CAPTURE=1`, prompt "Reply with exactly the single word: KIWI" | scroll `agy_<conversationId>` with 2 messages: User "Reply with exactly the single word: KIWI", Assistant "KIWI" |
| same plus `MEMB_NO_MEMORY=1`, prompt "... PLUM" | no scroll (0 messages) |

Read back with `crates/memb/tests/hooks.rs::t013_read_scroll` (ignored helper test).

## Codex: not run yet

Codex's usage quota is used up until 2026-11-28 21:23 local (see T025 transcript). A prompt would still reach the
`UserPromptSubmit` hook, but the `Stop` hook needs a finished model reply. Also needs `/hooks` trust.

## Claude Desktop (Code tab, installed plugin 0.5.0): VERIFIED live

Method: the user installed `memb.plugin` (0.4.0 build with hooks, later reinstalled as 0.5.0) in Claude Desktop. This
project's local settings got a temporary `env` block (`MEMB_SCROLL_CAPTURE=1`, `MEMB_DATA_DIR=C:\tmp\t013\cc\data`;
the original file was backed up and restored afterwards, the temporary data directory was deleted). The hooks that ran
are the plugin's own `hooks/hooks.json` (`${CLAUDE_PLUGIN_ROOT}/server/memb.exe` with `args`), so the
plugin-root substitution and the direct (no shell) command are confirmed for Claude Desktop.

| Run | Result read back from the data dir (session `cc_<session id>`) |
|---|---|
| capture on | 2 messages: Assistant (the previous reply, from `Stop`) and User (the next prompt, from `UserPromptSubmit`), text exact |
| then `MEMB_NO_MEMORY=1` added to the same env block; one more reply and one more prompt | still 2 messages: nothing new stored |

## Codex: NOT run (own ticket)

Codex's usage quota is used up until 2026-11-28 21:23 local (see the T025 transcript), so a model-driven session is
impossible now; a live Codex check also needs the user to trust the hooks in `/hooks`. Moved to its own ticket (T027).
