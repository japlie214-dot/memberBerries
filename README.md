# memberBerries (`memb`)

A Windows-first, local project memory tool shared across assistants. Save a reviewed note in one client and recall it from another running under the same Windows account and project. No background capture, network listener, model download, or API key is required.

## Get started

Use `memb.plugin` for Claude Cowork, `memb.mcpb` for Claude Desktop Chat, and the separate AGY and Codex archives for those clients. See [installation and smoke tests](wiki/Install.md) for manual stdio setup and the verification status of each host. Native import is not implied by a working MCP runtime.

Start with a user-approved topic note, then add knowledge linked to a topic and claims linked to knowledge. Search, inspect, or explicitly forget a note under the same project slug. See the [tool guide](wiki/Memory.md) for examples and privacy limits.

## Build on Windows

```powershell
cargo test --workspace --locked
cargo build --release --locked
python -I scripts/package.py
```

Archives and checksums are written to `dist/`. Source and packaging cross-checks, design details, and troubleshooting are in the [wiki](wiki/Home.md). The [phase ledger](wiki/Phase-Coverage.md) records which original designs are shipped, replaced, or still open. The original phase folders are local inputs, not part of the release.
