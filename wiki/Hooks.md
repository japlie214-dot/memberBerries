# Hooks: opt-in chat capture and retrieval

Hooks let memb record short rolling "scrolls" of your conversations and, on supported hosts, offer relevant notes back to the model. **Everything is off by default.** The packages ship the hook files, but `memb.exe hook` does nothing, reads no input and creates no database unless the environment of the host process contains `MEMB_SCROLL_CAPTURE=1`.

## Switches

| Variable | Effect |
|---|---|
| `MEMB_SCROLL_CAPTURE=1` | Required to capture anything. Set it in the environment the host is started from. |
| `MEMB_NO_MEMORY=1` | Kill switch: the hook exits without reading input. |
| `MEMB_INTERNAL=1` | Set by `memb curate` in the environment of the model command it starts, so that command's own session is never captured (provided its host runs memb's hooks with that environment, which depends on the host and has not been verified). The hook exits without reading input when it is set, and `memb curate` refuses to run when it is set. |
| `scroll.ignore_dirs` (config.toml) | Patterns such as `*/tmp/*`: a working directory whose lower-cased path contains the pattern (without `*`) is never captured. See [Configuration](Configuration.md). |
| `MEMB_IGNORE_DIRS` | Absolute directories (OS path separator between them). Prompts from a working directory inside one are not captured. A relative or unresolvable entry disables capture, because the exclusion cannot be trusted. |

Messages over 16 KiB, empty messages and anything matching a credential pattern are dropped, not truncated. Session ids must be 1 to 128 characters of letters, digits, `-`, `_` or `.`; the host working directory must be an absolute, existing path. Whatever the payload, the hook exits 0; only a malformed command line (wrong arguments) exits 2. When it rejects an input as invalid, or cannot open the store, it prints a short notice to stderr (one line, or two when an invalid `config.toml` is the cause: the config message, then the skip notice). Skips that are intentional or that fail a gate (a kill switch, an excluded directory, a missing, relative or unresolvable working directory, an Antigravity event that is not fully idle, a repeated Stop for the same turn) print nothing, except that the config is loaded before most of these gates: with an invalid `config.toml` such a skip still prints the config message and the skip notice (only the switch checks, which exit before any input or config is read, stay silent).

## What is stored

Each session keeps a live scroll of messages. When a newly arrived message brings a scroll to 90 messages, those 90 (including the new one) become a numbered chunk and the last 10 of them stay in the live scroll as overlap. Chunks are the raw evidence the curator reads (`scroll_chunk_read` returns bounded pages) and contain private conversation text, so treat the data directory accordingly. Retrieval deduplicates by session: a note is injected once per "epoch", where the epoch is the session's chunk count divided by `scroll.dedup_reset_every_n_chunks` (default 7, rounded down), so it can be injected again after as few as 1 new chunk (when the count crosses a multiple of that number) and at most that many.

## Per host

| | Claude Code / Cowork | Codex | Antigravity (`agy`) |
|---|---|---|---|
| Package file | `hooks/hooks.json` in `memb.plugin` | `hooks/hooks.json` in `memb-codex.zip`, referenced by the `hooks` key of `.codex-plugin/plugin.json` | `hooks.json` at the root of `memb-agy.zip` |
| Events used | `UserPromptSubmit`, `Stop` | `UserPromptSubmit`, `Stop` | `Stop` only (no prompt event exists) |
| Command | `${CLAUDE_PLUGIN_ROOT}/server/memb.exe` with `args` (Claude Code's hooks documentation says a command with `args` is run directly, without a shell) | `"${PLUGIN_ROOT}/server/memb.exe" hook ...`, run by the host through `cmd.exe /C` on Windows | `server\memb.exe hook assistant-stop --cli agy`; a relative path, so it needs the host to run it from the plugin root (Antigravity's documentation says it does; not observed, see Verification status) |
| Captures | `prompt` and `last_assistant_message` | `prompt` and `last_assistant_message` | The last finished turn, read from the transcript |
| Retrieval | Yes: prompts of 100+ characters get untrusted context as `hookSpecificOutput.additionalContext` | Yes: the same context as plain stdout, which Codex adds as developer context | No |
| Extra step | None. Hooks load in Cowork and the Code tab, not in claude.ai chat. | Codex only runs hooks you have reviewed: open `/hooks` and trust them. A changed hook must be re-trusted. | None documented. |

Capturing a message prints nothing on stdout. The only stdout is the retrieval context that `prompt-submit` prints for Claude Code and Codex when it injects notes (never on `Stop`, never for Antigravity). That matters: Claude Code and Codex add `UserPromptSubmit` stdout to the model context, and Codex requires any `Stop` output to be JSON, so a `Stop` hook must stay silent.

### Antigravity details

Antigravity's `Stop` payload (`conversationId`, `workspacePaths`, `transcriptPath`, `fullyIdle`, ...) carries no message text, so memb reads the end of its own transcript instead. It acts only when `fullyIdle` is true, accepts only a transcript at `.../.gemini/antigravity-cli/brain/<conversationId>/.system_generated/logs/transcript.jsonl` (any other path from the payload is refused), reads at most the last 2 MiB, and ignores truncated records. A turn is the last model answer without tool calls plus the user input before it; a repeat of the same final answer is not stored twice. The record layout (`source`, `type`, `status`, `content`) was observed in real transcripts and is not in Antigravity's documentation, so a future Antigravity change can silently stop capture; the hook then does nothing rather than guessing.

## Retrieval rules

- Only the latest user message is used, cut at 512 bytes, and only when it is at least `scroll.min_prompt_chars` (100) characters.
- A **slash command** (the message starts with `/` followed directly by a letter, digit, `_` or `-`, such as `/help` or `/review ...`) never triggers retrieval, however long. It is still recorded in the scroll.
- Notes are not repeated: a note injected into a session is not injected again in the same epoch (see `scroll.dedup_reset_every_n_chunks`), **unless it was updated after it was injected**: then it comes once more, with its new text, and not again until it changes again.
- Query cache: the 256-entry query LRU lives in the long-running MCP server only. A hook is a new process each time, so it would never hit, and a disk cache would add writes to every prompt for nothing measurable.

## Hook latency (T019, 2026-11-26)

Release build, this machine (Windows 11), 500 notes, 30 runs each, wall time including process start:

| Run | median | p90 |
|---|---|---|
| `memb --version` (process start only) | 80 ms | 101 ms |
| hook with capture off (exits at once) | 74 ms | 93 ms |
| hook with capture on, short prompt (no retrieval) | 76 ms | 93 ms |
| hook with capture on, slash command | 73 ms | 92 ms |
| hook with capture on, 100+ character prompt (search plus dedup) | 74 ms | 95 ms |

Starting a 26 MB executable takes about 75 ms here (most of it Windows process creation and scanning), more than the whole hook, so the plan's "under 8 ms" cannot be shown for the process and is not claimed. What can be said: the hook's own work (capture, search of 500 notes, deduplication) is below the measurement noise of a few milliseconds, because every row is within about 6 ms of the others.

## Verification status

Verified here: the hook binary against synthetic payloads in each host's documented shape (`crates/memb/tests/hooks.rs`: capture, silence on stdout, every kill switch, secret and size rejection, exclusion directories, per-host retrieval output, deduplication, Antigravity transcript rules); the packaged files parse and name the packaged executable; `agy plugin validate --strict` accepts the Antigravity package; `claude plugin validate` (Claude Code 2.1.330) lists both hooks of the Cowork package and accepts its manifest with one warning about missing author details (it also prints marketplace-submission findings, which do not affect a local install).

Verified live (T013): Antigravity 1.5.15 fires the Stop hook and memb stores the turn (see evidence/t013-hosts.md); the live run found and fixed two wrong assumptions (transcript file name and the wrapped user text). Not verified: a real model-driven session in Claude Desktop or Codex firing these hooks. For Antigravity, that the host runs the hook with the plugin root as its working directory (the Antigravity documentation says so and `hooks.json` sets no `cwd`; nobody has observed it, and the relative `server\memb.exe` path depends on it). Also for Antigravity, the MCP entry uses `"cwd": "${PLUGIN_ROOT}"` with a relative `command`; the plugin validator accepts it, but nobody has observed the host expanding that variable in `cwd` at run time (the Antigravity docs and its validator disagree about where `${PLUGIN_ROOT}` expands). Codex's Windows command handling and plugin-root substitution come from its open-source code, not from a run of Codex 0.156.0. The original assembly kit's Codex TOML (`prompt_submit = [...]`) and Antigravity JSON (`UserPromptSubmit` strings) do not match either host's documented format and were not used.

## Manual route

To enable hooks without the plugin files, copy the entries from the package's hook file into the host's own configuration (`~/.codex/hooks.json`, `.agents/hooks.json` or `~/.gemini/config/hooks.json`, or Claude Code settings) and replace the plugin-root variable with the absolute path of `memb.exe`.
