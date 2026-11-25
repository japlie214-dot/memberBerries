# memberBerries (`memb`) v1.0 — Complete IKEA Assembly Manual (Rev 2)

> **Starting State (per Kanban `memberberries/release/T006`):**
> - Your local `build/memb` checkout already has the `initialize` MCP UI extension test passing (`cargo test --workspace` + `cargo build --release`).
> - No existing `memory.redb` data has been migrated or deleted.
> - **Final UI Decision:** The user has decided to **drop MCP App (`ui://` / `open_inspector`)** due to Claude Desktop's `localMcpBridge` stripping `_meta.ui` on both plugins and `.mcpb` extensions, and standardize exclusively on **`memb.exe web`** for the local browser inspector. (You may either keep the existing `initialize` test harmlessly or remove `open_inspector`/`ui://` cleanly in Step 1.)

---

## Critical Fixes Applied in Rev 2 (Addressing T006 Audit Findings)

1. **Fail-Closed `projects_v1` Migration (`crates/memb-store/src/lib.rs`):**
   - `LegacyProject` now reads `next: u64` (matching the live v0.3.0 `projects_v1` JSON, with `#[serde(alias = "next_id")]` fallback).
   - Deserialization errors **abort the entire migration transaction** (`return Err(...)`)—never silently skipping a project and never marking `migrated_v1_to_v2` unless 100% of legacy notes migrate and `migrated_notes == expected_notes`.
   - `projects_v1` is preserved untouched as a read-only backup.
   - Includes two unit tests: lossless migration of `{"next": 3, "notes": {...}}` AND fail-closed abort on corrupt JSON.
2. **Real OS Exclusive File Locking (`acquire_os_lock`):**
   - Uses Rust 1.89's stabilized `std::fs::File::try_lock()` with 25ms backoff up to 3s and an `OsLockGuard` that calls `unlock()` on `Drop`, held across `open()` and every write mutation.
3. **Complete Crate Manifests & Implementations for Boxes B–E:**
   - Every crate (`memb-scroll`, `memb-win`, `memb-embed`, `memb-rag`, `memb-curator`, `memb-ops`) now ships with both `Cargo.toml` and complete compilable `src/lib.rs` + unit tests—no stub comments or missing manifests.

---

## Step-by-Step Assembly Order

### Box A — `v0.4.0` Core Cleanup (Unified Graph + `desc` + `properties` + Safe Migration)
1. **Back up** `%LOCALAPPDATA%\memberBerries\memory.redb` (if present) before running live tests.
2. Copy `box-a-v0.4-core-cleanup/crates/memb-core/src/lib.rs` and `box-a-v0.4-core-cleanup/crates/memb-store/src/lib.rs` into your repo.
3. Update `crates/memb/src/main.rs` to remove `Scope` / `MEMB_PROJECT` / `project` tool args, add `update`, `supersede`, `retract` MCP tools, and drop `open_inspector` / `inspector_*` MCP tools (keeping `memb.exe web` only).
4. Copy `box-a-v0.4-core-cleanup/crates/memb/src/viewer.rs`, `viewer.html`, and `packages/cowork/skills/memb/SKILL.md`. Update `crates/memb/src/web.rs` to remove `/api/projects` and route `/api/graph` & `/api/snapshot` directly to `viewer::graph` / `viewer::snapshot`.
5. Update root `Cargo.toml` per `workspace/Cargo.toml.v0.4`.
6. **Gate A (must pass before Box B):**
   ```powershell
   cargo test --workspace
   cargo build --release
   ```
   Confirm `migrates_legacy_projects_v1_with_next_field_losslessly` and `aborts_migration_on_corrupt_legacy_project_without_marking_complete` both pass.

### Box B — Phase 1: Scrolls, `total_chunks / 7` Dedup, Win32 2 GiB Cap & Hooks
1. Copy `box-b-phase1-scrolls-and-hooks/crates/memb-scroll` and `crates/memb-win` into `crates/`.
2. Add both crates to root `Cargo.toml` `members` and `crates/memb/Cargo.toml`.
3. Add CLI subcommand `memb.exe hook <prompt-submit|assistant-stop> --cli <cc|codex|agy>` calling `memb_scroll::is_memory_disabled`, `append_message`, `should_retrieve_for_prompt`, and `should_inject_and_mark(..., 7)`.
4. Expose read-only MCP tool `scroll_chunk_read(chunk_id, offset?, limit?)` calling `memb_scroll::read_chunk`.
5. Call `memb_win::attach_job_object_2gb_cap()` on `memb.exe daemon` startup.
6. **Gate B:** `cargo test --workspace` passes (including `prompt_gate_enforces_100_chars` and `rollover_at_90_retains_last_10_and_resets_dedup_at_7_chunks`).

### Box C — Phase 2: EmbeddingGemma 2 + `i8` Hot Set + Hybrid RAG
1. Copy `box-c-phase2-embeddings-and-hybrid-rag/crates/memb-embed` and `crates/memb-rag` into `crates/`.
2. Wire `memb_rag::hybrid_search` into the `search` MCP tool and hook retrieval path (falling back automatically to lexical search when `llama-server.exe` / `embeddinggemma-2.gguf` is absent).
3. **Gate C:** `cargo test --workspace` passes; `memb.exe search` works both with and without `llama-server.exe`.

### Box D — Phase 3: Layer 1/2 Curation & Provenance
1. Copy `box-d-phase3-curation-and-llm/crates/memb-curator` into `crates/`.
2. Wire Layer 2 clustering (`build_evidence_packets`) and `apply_curator_action` into the daily curation pipeline, spawning configured CLI backends (`claude -p` / `codex exec` / `agy -p`) with `MEMB_INTERNAL=1`.
3. **Gate D:** `cargo test --workspace` passes; verify `properties.source_chunks` is stamped on curated notes.

### Box E — Phase 4: 15:45 WIB Routine, Rollback, Obsidian Mirror & `v1.0.0`
1. Copy `box-e-phase4-scheduler-and-ops/crates/memb-ops` into `crates/`.
2. Add CLI subcommands:
   - `memb.exe daemon` (60s loop after 30s boot settle evaluating `memb_ops::should_run_daily_routine`, sweeping idle scrolls `> 24h` with `>= 45` msgs, running Layer 2 curation, and syncing Obsidian mirror).
   - `memb.exe rollback --rev <rev_id>` calling `memb_ops::rollback_revision`.
   - `memb.exe mirror --out <dir>` calling `memb_ops::sync_obsidian_mirror`.
3. Update root `Cargo.toml` per `workspace/Cargo.toml.v1.0`.
4. **Final v1.0.0 Gate:**
   ```powershell
   cargo test --workspace
   cargo build --release
   .\target\release\memb.exe web
   ```
   Install the background task via `box-e-phase4-scheduler-and-ops/deploy/install-windows-task.ps1` and mark Kanban ticket `memberberries/release/T006` complete.
