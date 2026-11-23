# Internals and operating boundaries

## Runtime flow

A host spawns `memb.exe mcp` and sends newline-delimited JSON-RPC over stdin/stdout. Initialization negotiates a supported MCP protocol and tools are unavailable until the initialized notification. Invalid messages get protocol errors; valid tool calls with invalid inputs get an MCP tool error. Frames are held to 64 KiB; oversized lines are drained without storing their contents. Stdout carries only protocol data. Startup failures use a fixed stderr message.

The runtime resolves scope from `MEMB_PROJECT` and/or a validated request slug. Domain checks in `memb-core` enforce bounds and credential rejection before `memb-store` receives a valid note. Each process accesses the same `%LOCALAPPDATA%\memberBerries\memory.redb` (or absolute `MEMB_DATA_DIR`). MCP mode starts no network listener, permanent broker, or external model. The separately started [web inspector](Inspector.md) uses a private, read-only IPv4 loopback listener. The store acquires an exclusive OS file lock in a stable `store.lock` alongside the database for each operation. It waits up to roughly three seconds for contention. The lock is released even when the process exits unexpectedly; do not manually remove the lock file while any client is active.

`redb` handles atomic writes. The versioned table key is the project slug; its value is serialized project state with a monotonic note counter and the note map. The store limits each project to 500 notes and tracks at most 128 project records. An empty project can retain an ID counter, so deleting all notes does not recycle IDs. Unapproved writes and rejected credentials do not create a store. Searching before the first write returns no notes without creating a database. Logical deletion removes the note from active queries; it does not guarantee physical erasure from storage pages, OS backups, or host transcripts.

## Ranking and graph

Search splits text into Unicode alphanumeric words, lowercases them, and counts distinct matching terms (three points for title, one for body). It takes up to five direct hits by descending score and ascending ID, then adds claim parents or claim children of knowledge notes as space permits, to eight total. Expansion uses matching nodes only and does not recurse or fan out from a topic. The design is deterministic lexical retrieval, not BM25, embedding similarity, or hybrid RRF.

## Privacy and recovery

The OS account and data-directory ACL are the storage boundary; do not point `MEMB_DATA_DIR` at shared, cloud-synced, or untrusted folders. Any trusted process running as that account can potentially access the database. Calls to a host model can send retrieved text to that host, even though memb itself is local. Review text and scope before approving a write; when testing, isolate the data directory.

Close all clients before creating or restoring a directory backup. Do not hand-edit `memory.redb` or copy it while there is an active writer. If it cannot be opened, first confirm the configured directory and write permissions and check for concurrent profile changes. Do not overwrite a damaged store with an empty one; retain the closed store for recovery. There is no scheduled export or compaction guarantee.

## Build pipeline

`memb-core` owns types and validation; `memb-store` owns persistence, graph checks, and ranking; `memb` owns the host protocol. `cargo test --workspace --locked` runs domain and subprocess tests. `cargo build --release --locked` builds the Windows runtime. `python -I scripts/package.py` uses an explicit archive allowlist and emits checksums. Re-running packaging with an identical binary and the same toolchain is deterministic; compiler outputs from unrelated build environments need not match byte for byte.
