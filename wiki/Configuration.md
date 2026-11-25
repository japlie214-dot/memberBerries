# Configuration (`config.toml`)

memb runs without any configuration. To change a limit, put a `config.toml` next to the database: `%LOCALAPPDATA%\memberBerries\config.toml`, or inside the absolute directory named by `MEMB_DATA_DIR`. [config.example.toml](../config.example.toml) lists every supported setting with its default.

## Rules

- **Precedence:** an explicit environment variable wins, then the file, then the built-in default.
- **Strict:** an unknown key, a wrong type, an out-of-range value or a file larger than 64 KiB stops the command (a hook skips its event instead) with a message such as `memb: scroll.max_messages must be within 10..=1000` (or `memb: config.toml is not valid at line 2: invalid type, expected usize`). Messages name the setting (and the line, for a syntax or type error in the file) but never repeat the value you wrote, whether it is text or a number. They may repeat names you wrote: an unknown key, or an environment variable inside a path. A bad file never creates a database.
- **Applies everywhere:** the MCP server, `memb search`, hooks, `memb daemon` and the other commands all read the same file at start. Restart a running client or daemon after editing.
- **No secrets.** The file is not scanned for credentials.
- **Paths** may use `%NAME%` environment variables (for example `%LOCALAPPDATA%`); the result must be absolute and an unset variable is an error.

## Settings

| Setting | Default | Range / values | Effect |
|---|---|---|---|
| `storage.max_notes` | 50000 | 1 to 50000 | Refuses new notes (`remember`, `supersede`) once this many notes are stored; retracted and superseded notes still count. Existing notes are never removed. It can lower the built-in limit, not raise it. |
| `scroll.max_messages` | 90 | 10 to 1000 | When a newly arrived message brings a scroll to this many messages, those messages (including the new one) become a chunk. |
| `scroll.retain_tail` | 10 | 1 to `max_messages / 2` | Messages of the finished chunk kept in the live scroll as overlap. |
| `scroll.min_prompt_chars` | 100 | 1 to 10000 | Shorter prompts never trigger retrieval. |
| `scroll.dedup_reset_every_n_chunks` | 7 | 1 to 1000 | A note already injected in a session may be injected again once the session's chunk count crosses the next multiple of this number, which is between 1 and this many new chunks later. |
| `scroll.ignore_dirs` | none | up to 64 non-empty patterns of at most 512 bytes | A working directory whose normalized, lower-cased path contains a pattern (with `*` removed, so `*/tmp/*` means `/tmp/`; a pattern made only of `*` matches nothing) is never captured or read. `MEMB_IGNORE_DIRS` (absolute directories) is an additional, stricter mechanism; see [Hooks](Hooks.md). |
| `embeddings.enabled` | true | true, false | `false` forces lexical-only search even if a port or model directory is set in the file. |
| `embeddings.port` | not set | 1 to 65535 | Loopback llama-server to ask for vectors. **This sends note text and queries to that service.** Same as `MEMB_EMBED_PORT`. |
| `embeddings.model_id` | not set | up to 128 characters of letters, digits and `. _ : - @` | Names the vector cache for the server path. Same as `MEMB_EMBED_MODEL_ID`. |
| `embeddings.gemma_model_dir` | not set | absolute path | In-process EmbeddingGemma 2 (ONNX q4) directory. Same as `MEMB_GEMMA_MODEL_DIR`. |
| `embeddings.mrl_dim` | full width | 128, 256, 512, 768 | Shorter Matryoshka vectors. Same as `MEMB_EMBED_MRL_DIM`; see [Internals](Architecture.md). |
| `curator.cli_command` | `codex exec -m gpt-6-luna -c model_reasoning_effort=medium -s read-only --skip-git-repo-check --ephemeral` | 1 to 16 words of at most 256 bytes | The command `memb curate` runs, with conversation text on its standard input. It is split on whitespace and started **without a shell** (no quoting or pipes), so a program that is not an `.exe` on the search path (for example an npm `.cmd` shim) must be given as `cmd /C ...`. **Whatever it is, memb sends it private conversation text**; use a command you trust. See [Operations](Operations.md). |
| `curator.daemon_enabled` | false | true, false | Lets `memb daemon` run curation once per routine run. **Off by default**: turning it on means conversation text is sent to `curator.cli_command` without you running a command. Proposals still wait for your approval unless `curator.auto_apply` is on. |
| `curator.auto_apply` | false | true, false | Full auto-apply: valid proposals of every kind (create, claim, supersede, update, retract) are applied without a typed confirmation, each stamped with its `source_chunks` and recorded so `memb proposals rollback <id>` reverses it. Rails: at most `curator.max_auto_edits` per run, a retraction that would cascade is never automatic, the kill switch `memb proposals pause` stops it, and the secret check still applies. Applies to `memb curate` and the daemon. |
| `curator.max_auto_edits` | 5 | 1 to 50 | Most proposals one run applies automatically; the rest stay pending for `memb proposals apply`. |
| `curator.backend` | `cli` | `cli` | The only supported backend. |
| `curator.staging_ttl_days` | 30 | 1 to 365 | Staged evidence and pending proposals older than this expire. Raw chunk text is not deleted. |
| `curator.chunk_to_chunk_threshold`, `curator.chunk_to_node_threshold` | 0.72, 0.70 | fixed | Accepted only at these values (the setting is checked but never read: the `curator_evidence` tool uses built-in constants of the same value, and `memb curate` does not cluster). |
| `scheduler.routine_wib_hour`, `routine_wib_minute` | 15, 45 | 0 to 23, 0 to 59 | WIB (UTC+7) time of day from which the daily routine becomes due, once the cooldown has passed. After the catch-up window (24 hours, or the cooldown if longer) it is due at any hour. |
| `scheduler.cooldown_hours` | 20 | 1 to 168 | Minimum gap after the last successful run. The catch-up window is 24 hours or the cooldown, whichever is longer. |
| `scheduler.idle_sweep_hours` | 24 | 1 to 720 | Scrolls idle longer than this are chunked by the daemon. |
| `scheduler.idle_sweep_min_messages` | 45 | 1 to 10000, and greater than `scroll.retain_tail` | Smaller scrolls are left alone. |
| `scheduler.obsidian_mirror_dir` | not set | absolute, existing directory | Where the daemon writes a fresh `mirror-<timestamp>` folder each run. `memb daemon --mirror-out <dir>` overrides it. |

## Settings from the original kit that are not accepted

| Kit key | Why |
|---|---|
| `embeddings.binary`, `embeddings.model_path` | memb never starts an embedding server. Start `llama-server` yourself and set `embeddings.port`, or use `embeddings.gemma_model_dir`. These keys fail with a message saying so. |
| `curator.backend` other than `cli`, or curator thresholds other than 0.72 and 0.70 | Only the command-line backend exists, and the thresholds are fixed. Other values fail with a message instead of being ignored. |
| `storage.max_notes` above 50000 | The built-in limit is a hard cap. |

## Privacy

Setting `embeddings.port` is an explicit opt-in to send note text and search queries to whatever listens on that loopback port; the example file leaves it commented out. `embeddings.enabled = false` is the way to guarantee that nothing is sent: it forces lexical-only search even when a port or model directory is set, in the file or in the environment. The same switch exists as the environment variable `MEMB_EMBED_DISABLE=1`; an explicit `MEMB_EMBED_DISABLE=0` overrides `enabled = false` from the file.
