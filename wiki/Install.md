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

v0.6.0: `scripts/smoke-packages.py` runs the three packaged binaries as separate stdio processes against one data directory (remember, search, update, retract, inspect) and asserts the exact ten-tool list, so `open_inspector` and the old project tools cannot ship unnoticed. Real-host installs of the v0.4 packages (Claude Desktop, Codex, AGY) are not yet verified; earlier checks covered v0.1.0 only (Cowork import and a native `search` call, Codex CLI 0.156.0 manual registration in an isolated profile, AGY 1.5.14 CLI syntax). The MCP App panel was dropped because Claude Desktop strips the UI metadata; use `memb.exe web` instead.

### Cross-client smoke test

1. Use only synthetic text and a fresh, absolute `MEMB_DATA_DIR` shared by both hosts.
2. In the first host ask it to show and save one `topic` with title `Orchard checklist` and description `Check the hose each Friday`. Confirm only that exact content.
3. In the second host, search for `orchard`. Confirm that the exact same ID and description are returned, including after a host restart.
4. Retract the note after checking its ID and confirming. In either host, inspect the ID again; its status should be `retracted`. Retraction is a soft delete, not physical erasure.
5. If no tools appear, verify the path, `memb.exe --version`, startup environment and workspace trust; if results are empty, compare `MEMB_DATA_DIR` in both hosts. Do not enable unauthenticated HTTP to troubleshoot.
