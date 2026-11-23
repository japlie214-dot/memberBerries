# Install and verify

Windows x64 is the first target. The four archives contain the same executable. Use one Windows account and keep all clients on the same data directory; otherwise they cannot share notes. No archive installs a scheduled task, model, API client, or hook. Keep the executable in a stable directory; do not run it directly from the ZIP viewer.

| Client | Package | Setup |
|---|---|---|
| Claude Desktop Cowork | `memb.plugin` | Use the Cowork plugin import UI if available. The skill and MCP registration are bundled. If tools do not appear, the current Cowork sandbox or policy may not let it launch a Windows process. Do not expose local memory to the network to bypass this. |
| Claude Desktop Chat | `memb.mcpb` | Open the extension file in Claude Desktop, review its local MCP permissions, and enable it. If manual local MCP is supported, add `mcpServers.memb` to `%APPDATA%\Claude\claude_desktop_config.json`, then restart Desktop. |
| Antigravity | `memb-agy.zip` | Extract to a stable directory. For the predictable manual route, merge `mcpServers.memb` into `.agents/mcp_config.json` in a trusted workspace or `~/.gemini/config/mcp_config.json` for the account. Use the absolute executable path. |
| Codex | `memb-codex.zip` | Extract to a stable directory. Register it with `codex mcp add memb -- 'C:\Tools\memb\server\memb.exe' mcp`, or add `[mcp_servers.memb]` to a trusted project's `.codex/config.toml`. The ZIP is not consumed by Claude's import UI. |

### Manual configuration and removal

Use this JSON entry inside the client's existing `mcpServers` object. Double backslashes are JSON escaping, not part of the Windows path. Replace the path with the actual location. Do not replace the entire config file.

```json
{"memb":{"command":"C:\\Tools\\memb\\server\\memb.exe","args":["mcp"],"env":{"MEMB_PROJECT":"orchard-demo"}}}
```

The same fields in Codex TOML:

```toml
[mcp_servers.memb]
command = 'C:\Tools\memb\server\memb.exe'
args = ['mcp']

[mcp_servers.memb.env]
MEMB_PROJECT = 'orchard-demo'
```

To uninstall, remove only the `memb` extension/plugin or configuration entry and restart the host. For manual Codex registration use `codex mcp remove memb`. Removing the integration does not remove memory. For upgrades, close connected clients, keep a backup of the store, and replace the old binary/package with the new version.

### Verification matrix

The Windows runtime passed an actual stdio MCP handshake and tool round-trip across separate processes, including restart and logical deletion. Native Cowork import and access to the Windows executable, Desktop extension import, and end-to-end AGY/Codex plugin discovery and model-mediated tool use require separate host smoke tests. An installed CLI version or a valid archive alone does not prove integration.

### Cross-client smoke test

1. Use only synthetic text in a fresh project, for example `orchard-demo`. Pin or explicitly specify the same scope in both hosts.
2. In the first host ask it to show and save one `topic` with title `Orchard checklist` and body `Check the hose each Friday`. Confirm only that exact content.
3. In the second host, search for `orchard`. Confirm that the exact same ID and body are returned, including after a host restart.
4. Forget the note after checking its ID and confirming the deletion. In the first host, inspect the ID again; it should be absent.
5. If no tools appear, verify the path, `memb.exe --version`, startup environment and workspace trust; if results are empty, compare scope and `MEMB_DATA_DIR` in both hosts. Do not enable unauthenticated HTTP to troubleshoot.
