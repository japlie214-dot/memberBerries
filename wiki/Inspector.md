# Read-only inspector

The v0.4 inspector displays the **unified** local memory graph in a browser. Project isolation and the project selector from v0.3 are no longer present: anyone with access to the inspector can see notes migrated from every legacy project in that data directory. Use search and page controls to navigate notes; graph links are displayed only when both endpoints are on the current page. Select a node to inspect its description, properties and parent IDs. Drag to pan, scroll or use the zoom buttons, and use the list as an alternative to the graph. Refresh reloads the data; the viewer cannot modify notes.

## Local web app

Run the Windows executable from a directory you control:

```powershell
.\memb.exe web
```

Open the entire URL printed in the terminal, including its private fragment. Use `web --port 0` to select an available port. Stop the process with Ctrl+C. Do not share the URL, proxy the server, or expose its port. The server binds to IPv4 loopback, checks host and origin, and requires a random per-process access key for API reads. `MEMB_DATA_DIR` selects the local data directory; `MEMB_PROJECT` no longer restricts access.

Opening a legacy data directory initiates a schema migration. Stop all old Memb clients and make a verified backup first. The migration preserves the legacy table and records legacy project and ID metadata, but IDs in the unified graph may differ. Older clients must not write the old table afterward: the new store detects legacy changes and refuses access rather than silently diverging. Test recovery using copies before relying on a migration.

The current implementation uses the local browser inspector rather than the earlier MCP App panel. Host integration and release packaging for this version are not yet verified.
