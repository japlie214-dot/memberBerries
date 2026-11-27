# T029 cheap knowledge-base version check (release build, 2026-11-27)

Command: `cargo test -p memb-store --release --locked --offline -- --ignored --nocapture version_check_cost`
(500 notes, 50 calls averaged per row, release profile).

**Machine state:** NOT idle in either round. I waited 902 s (round 1) and 603 s (round 2) for no cargo, rustc or memb process and one was always running (other workers' builds, a long-lived `memb` process), so both ran on a busy 12-core machine. Absolute numbers are noisy; compare within a round.

## Round 1: where the time of the original `kb_version()` went (each hot-set hit paid it)

| Step | Run 1 | Run 2 |
|---|---|---|
| OS lock | 1.7 ms | 2.3 ms |
| Open redb (`Database::create`, 16 MiB cache) | 25.8 ms | 30.2 ms |
| Legacy verification (handle already open) | 0.005 ms | 0.004 ms |
| Read `next_rev` (handle already open) | 0.002 ms | 0.002 ms |
| **Original `kb_version` (lock + open + verify + read)** | **33.9 ms** | **58.4 ms** |

The cost is opening redb. A long-lived handle is impossible: redb holds an exclusive file lock while a `Database` is open, which would block every other memb process.

## Round 2: after the QA fix (self-validating record)

| Step | Run 1 | Run 2 |
|---|---|---|
| Slow path (lock + open + verify + read + publish on close), unchanged knowledge base but record missing or invalid | 23.8 ms | 25.0 ms |
| **Fast path `kb_version` (stat of memory.redb + read of kb.version)** | **1.4 ms** | **1.6 ms** |
| `filter_active` for one id (one database open, now also publishes on close) | 29.0 ms | 30.4 ms |

Hot-set hit, version check part: about 34-58 ms before, about 1.5 ms after (round 1 and round 2 are different load levels, so read it as roughly 20-30x).

## Design (after kb-qa case C011)

The first version trusted `kb.version` whenever it parsed. That was wrong: a writer that died between commit and publishing, or a writer that never publishes (an older memb binary on the same data dir), left a value that long-lived readers kept trusting.

Now the file is self-validating:

- `kb.version` holds `v3 <version> <len of memory.redb> <mtime of memory.redb in ns>`, taken when the last database handle was closed, still under the store lock.
- Every handle from `Store::open_db` is a `StoreDb` guard. When it is dropped (lock still held) it reads `next_rev`, closes the database, stats it, and writes the file (temp file + rename). So every committing writer publishes in the same critical section as its commit. Closing matters: opening and closing redb rewrites its header and changes the mtime (measured), so the stamp must be taken after the close.
- `kb_version()` stats `memory.redb`; if the file parses and its (len, mtime) equals the current stat it returns the recorded version. Otherwise it takes the slow path: lock, open (with `verify_legacy`, unchanged), read `next_rev`, publish on close.
- Any failure to write or rename removes the file. If even the removal fails, the old record carries an older stamp that cannot match a newer database, so it is never trusted. Nothing panics. A reader holding the file open on Windows (rename failure) therefore only costs a slow path.
- Effect on other operations: any open of the database by any process invalidates the record until the closing handle republishes, which it does itself. A foreign process that opens, commits and closes always changes the stamp.

## Worst case and why it is acceptable

A change is missed only if a writer that does not publish (older build, or one that dies before closing) finishes opening, committing and closing redb with the file length identical and the final modification time equal to the recorded one. NTFS time stamps advance with the system clock tick (at most about 15.6 ms; measured steps here were 36-70 ms between successive operations). Opening redb alone measured 17-30 ms in a release build on this machine, so a full open + commit + close inside one tick with an unchanged length is not reachable on this platform; it is a bounded risk on a much faster clock/filesystem, not a correctness hole. A writer that publishes (every current build) is exact.

## Tests (crates/memb-store)

- `a_writer_that_dies_between_commit_and_publish_is_seen_and_the_file_is_repaired` (test-only hook skips the publish after the commit; the next `kb_version` sees the change and the file is repaired; later calls are answered from the file).
- `a_writer_that_never_publishes_is_still_seen` (child process with publishing disabled changes the notes twice).
- `a_note_change_from_another_process_is_seen_by_the_next_version_check` (publishing child process, plus a check that unchanged calls take no slow path).
- `a_missing_garbled_or_stale_version_file_falls_back_to_the_database` (missing, empty, garbage, wrong number of fields, wrong values, and an old valid-looking record put back after a database change).

Mutation result: with the stamp comparison disabled, 3 of these 4 fail (the child-process test with a publishing writer passes either way, as expected).
