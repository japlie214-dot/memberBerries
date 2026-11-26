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

## Claude Desktop (Cowork and Code tab): not run yet

The installed memb plugin in Claude Desktop is the 2026-11-24 build without hooks. A live check needs either
installing the rebuilt `memb.plugin` into Claude Desktop with `MEMB_SCROLL_CAPTURE=1` in its environment, or temporary hook
settings in a project; both change the user's real configuration, so they wait for the user's approval.
