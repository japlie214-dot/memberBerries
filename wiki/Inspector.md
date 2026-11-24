# Read-only inspector

The inspector shows current counts by note type, expandable note text and parent IDs, a case-insensitive substring filter, and 12 notes per page. Use **Refresh page** for a new snapshot. Counts cover the whole selected project, even when you filter. The inspector has no edit/delete controls, no write routes, and no background polling. These are current counts, not historical metrics or a daemon health monitor.

## Local web app

Extract only `server/memb.exe` from `memb.plugin` to a directory you control. In that directory, run:

```powershell
.\memb.exe web
```

Open the entire URL printed in the terminal, including the private fragment. Enter a project slug and select **Load project**. Use `web --port 0` if the default port is occupied; the command prints the chosen port. Stop it with Ctrl+C. Do not share the URL, proxy the server, or expose the port. It binds to IPv4 loopback, checks the exact host and same-origin request context, requires a fresh random access key, and never allows cross-origin API reads. The key expires when the process exits. `MEMB_DATA_DIR` and `MEMB_PROJECT` apply as for stdio; the web app requires the entered slug to match a pinned project.

## MCP App in Cowork

Import or update to `memb.plugin` v0.2.1, then ask: “Use Memb's `open_inspector` tool for project `memb-smoke-test`; do not use search or another fallback.” `open_inspector` returns no notes to the model. A compatible host renders `ui://memb/inspector-v0.2.1.html` and routes page requests through the app-only `inspector_snapshot` tool. App-only metadata is a host visibility hint, not an authorization boundary. Use the web app if you want to inspect without a model host in the path.

Cowork v0.1.0 import and its native `search` call were verified by the user. That does not prove Cowork renders an MCP App from a local connector. In v0.2.0 the Code host returned the tool result without rendering a panel. Version 0.2.1 aligns resource association and visibility metadata with the working Calculator fixture; whether this resolves host rendering remains unverified. If Cowork shows only the tool reply rather than the interactive panel, use the web app; do not expose Memb over the Internet to work around that. The existing assistant `remember` and `forget` tools remain separate from the read-only inspector.

References: [Claude MCP Apps getting started](https://claude.com/docs/connectors/building/mcp-apps/getting-started), [MCP Apps specification](https://modelcontextprotocol.io/extensions/apps/overview).
