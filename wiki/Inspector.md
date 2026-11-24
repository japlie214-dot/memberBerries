# Read-only inspector

The inspector shows a read-only project selector and graph of topics, knowledge and claims, with edges to their parent nodes. Drag the board to pan, scroll or use the zoom buttons to zoom, and choose a node for its full text and parent IDs. A keyboard-accessible list provides an alternative to the graph. Use **Refresh** to reload; there is no background polling, editing or deletion. Counts are current notes, not historical metrics or a daemon health monitor.

If a project is pinned with `MEMB_PROJECT`, the selector contains only that project. Otherwise it lists stored project slugs; project names themselves can be sensitive, and the UI is intended for the current user only. The current project is preselected only when explicitly supplied to `open_inspector` or pinned; it cannot safely infer the active project from an unrelated filesystem path. An empty historical project may remain listed after its notes are forgotten.

## Local web app

Extract only `server/memb.exe` from `memb.plugin` to a directory you control. In that directory, run:

```powershell
.\memb.exe web
```

Open the entire URL printed in the terminal, including the private fragment. Enter a project slug and select **Load project**. Use `web --port 0` if the default port is occupied; the command prints the chosen port. Stop it with Ctrl+C. Do not share the URL, proxy the server, or expose the port. It binds to IPv4 loopback, checks the exact host and same-origin request context, requires a fresh random access key, and never allows cross-origin API reads. The key expires when the process exits. `MEMB_DATA_DIR` and `MEMB_PROJECT` apply as for stdio; the web app requires the entered slug to match a pinned project.

## MCP App in Cowork

Import or update to `memb.plugin` v0.3.0, then ask: “Use Memb's `open_inspector` tool for project `memb-smoke-test`; do not use search or another fallback.” `open_inspector` returns no notes to the model. A compatible host renders `ui://memb/inspector-v0.3.0.html` and routes page requests through the app-only `inspector_snapshot` tool. App-only metadata is a host visibility hint, not an authorization boundary. Use the web app if you want to inspect without a model host in the path.

Cowork v0.1.0 import and its native `search` call were verified by the user. That does not prove Cowork renders an MCP App from a local connector. In v0.2.0 the Code host returned the tool result without rendering a panel. Version 0.2.1 aligns resource association and visibility metadata with the working Calculator fixture; whether this resolves host rendering remains unverified. If Cowork shows only the tool reply rather than the interactive panel, use the web app; do not expose Memb over the Internet to work around that. The existing assistant `remember` and `forget` tools remain separate from the read-only inspector.

References: [Claude MCP Apps getting started](https://claude.com/docs/connectors/building/mcp-apps/getting-started), [MCP Apps specification](https://modelcontextprotocol.io/extensions/apps/overview).
