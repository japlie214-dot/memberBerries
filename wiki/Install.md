# Install and verify

Windows x64 is the first target. The archives contain the same executable. Claude Cowork only needs `memb.plugin`. Use one Windows account and keep all clients on the same data directory; otherwise they cannot share notes. No archive installs a scheduled task, model, or API client, and memb starts a model only when you run `memb curate` (see [Operations](Operations.md)). Hooks ship inside the packages but stay inert until you set `MEMB_SCROLL_CAPTURE=1` (see [Hooks](Hooks.md)); the `memb daemon` command is part of `memb.exe`, but no scheduled task or installer is packaged or registered (see [Operations](Operations.md)); an optional [configuration file](Configuration.md) changes limits. Keep the executable in a stable directory; do not run it directly from the ZIP viewer.

| Client | Package | Setup |
|---|---|---|
| Claude Desktop Cowork | `memb.plugin` | Use the Cowork plugin import UI if available. The skill and MCP registration are bundled. If tools do not appear, the current Cowork sandbox or policy may not let it launch a Windows process. Do not expose local memory to the network to bypass this. |
| Antigravity | `memb-agy.zip` | Extract to a stable directory. For the predictable manual route, merge `mcpServers.memb` into `.agents/mcp_config.json` in a trusted workspace or `~/.gemini/config/mcp_config.json` for the account. Use the absolute executable path. |
| Codex | `memb-codex.zip` | Extract to a stable directory. Register it with `codex mcp add memb -- 'C:\Tools\memb\server\memb.exe' mcp`, or add `[mcp_servers.memb]` to a trusted project's `.codex/config.toml`. The ZIP is not consumed by Claude's import UI. |

### Manual configuration and removal

Use this JSON entry inside the client's existing `mcpServers` object. Double backslashes are JSON escaping, not part of the Windows path. Replace the path with the actual location. Do not replace the entire config file. Optionally add an `env` object with an absolute `MEMB_DATA_DIR` to choose the shared data directory; there is no project setting any more.

```json
{"memb":{"command":"C:\\Tools\\memb\\server\\memb.exe","args":["mcp"]}}
```

The same fields in Codex TOML:

```toml
[mcp_servers.memb]
command = 'C:\Tools\memb\server\memb.exe'
args = ['mcp']
```

To uninstall, remove only the `memb` extension/plugin or configuration entry and restart the host. For manual Codex registration use `codex mcp remove memb`. Removing the integration does not remove memory. For upgrades, close connected clients, keep a backup of the store, and replace the old binary/package with the new version.

### Verification matrix

v0.10.0: `scripts/smoke-packages.py` runs the three packaged binaries as separate stdio processes against one data directory (remember, search, update, retract, inspect) and asserts the exact ten-tool list, so `open_inspector` and the old project tools cannot ship unnoticed. Real-host installs of the v0.4 packages (Claude Desktop, Codex, AGY) are not yet verified; earlier checks covered v0.1.0 only (Cowork import and a native `search` call, Codex CLI 0.156.0 manual registration in an isolated profile, AGY 1.5.14 CLI syntax). The MCP App panel was dropped because Claude Desktop strips the UI metadata; use `memb.exe web` instead.

### Cross-client smoke test

1. Use only synthetic text and a fresh, absolute `MEMB_DATA_DIR` shared by both hosts.
2. In the first host ask it to show and save one `topic` with title `Orchard checklist` and description `Check the hose each Friday`. Confirm only that exact content.
3. In the second host, search for `orchard`. Confirm that the exact same ID and description are returned, including after a host restart.
4. Retract the note after checking its ID and confirming. In either host, inspect the ID again; its status should be `retracted`. Retraction is a soft delete, not physical erasure.
5. If no tools appear, verify the path, `memb.exe --version`, startup environment and workspace trust; if results are empty, compare `MEMB_DATA_DIR` in both hosts. Do not enable unauthenticated HTTP to troubleshoot.

## Semantic search (EmbeddingGemma 2)

Out of the box memb searches lexically. For meaning-based search it can run Google's EmbeddingGemma 2 on your CPU, inside memb, with nothing sent anywhere. The model is not in the package (207 MB for the 4-bit build, 346 MB for the 8-bit build), so it is fetched once:

```powershell
memb models install
```

memb reads your machine (CPU features, physical cores, RAM) and picks the build that fits: a weaker machine gets the 4-bit build with 256-wide vectors, a machine with AVX2 or AVX-512, at least 8 physical cores and 16 GB gets the 8-bit build with 512-wide vectors. A machine without AVX2 with FMA (or AVX-512), or with under 8 GB of RAM, is told it cannot run the model well and nothing is installed; `memb models install --variant q4` overrides that. The command says what it will download and from where, and asks you to type `yes` (or pass `--yes`). It fetches three files from one pinned revision of `onnx-community/embeddinggemma-2-ONNX` on Hugging Face into `%LOCALAPPDATA%\memberBerries\models\embeddinggemma-2` (4-bit) or `...\embeddinggemma-2-int8` (8-bit), checks each file's size and SHA-256 against values built into memb, and makes a file visible only after it matches. An interrupted download resumes when you run the command again. A running memb server or host picks the model up when it restarts.

- `memb models status` shows this machine, the recommended build and what is installed (`--verify` also hashes the files); `memb doctor` reports it too.
- `memb models remove` deletes it; search is lexical again.
- To let memb do this on its own, set `embeddings.auto_download = true`: the daily routine then downloads the model once if none is installed and no other backend is chosen.
- An explicit choice always wins: `embeddings.gemma_model_dir`, `embeddings.port` or `embeddings.enabled = false`.
- The first search after the model appears embeds your existing notes once; after that only changed notes are embedded.
- It needs Windows' own `curl.exe` (present on Windows 10 and later).
