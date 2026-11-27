# T029 QA re-audit (kb-qa) - evidence

## T1 checkpoint
- Status: DONE for the checks run. Mutation (3) skipped, see Testing Ladder.
- Ticket: memberberries/release/T029, worktree C:\Projects\memberBerries\.claude\worktrees\agent-abdaeb3ef0b7ab58b
- lib.rs sha256 at audit start and after tests: 97be052fa3893280891d4a70a319bc1b0d25725fd1aded1b033d4d45b8c44102 (unchanged; not committed)
- Command: CARGO_TARGET_DIR=C:\Projects\memberBerries-swarm-target cargo test -p memb-store --locked --offline (sources touched first)
- Result: 17 passed, 0 failed, 1 ignored (version_check_cost); EXIT=0

## Ponytail Diff
- The change is confined to crates/memb-store/src/lib.rs. Hypotheses checked against the diff:
  1. A stale kb.version validating against a newer memory.redb. Every open_db handle is a StoreDb whose Drop publishes the version while the OS lock is still held (verified: all open_db call sites in lib.rs are preceded by acquire_os_lock; kb_version and Store::open drop the handle before the lock guard). The stamp (len, mtime) is taken after the redb handle closes, so the worker's own header rewrite is not a stale stamp.
  2. Non-publishing writer or crash between commit and drop. Only a commit whose final write lands in the same mtime tick as the last publish, with unchanged file length, can validate a stale record. Not proven unreachable: it rests on redb open taking longer than one NTFS tick (measured 17-30 ms on this machine; a faster disk may break it). Residual risk, recorded under Advisory Notes.
  3. Reader racing a replacement. The OS lock covers every open and publish, so a reader's stat either sees the old or the new stamp; a failed read or rename falls back to the slow path (remove_file on failure), never to a value the file does not carry.
- Minimality: the added code is the guard, the stamp check, and the tests. Validation of the legacy format and error propagation are kept (verify_legacy still runs on the slow path; parse failures return None).

## Testing Ladder
- unit: PASS. cargo test -p memb-store: 17 passed, 0 failed. The four named tests passed: a_writer_that_dies_between_commit_and_publish_is_seen_and_the_file_is_repaired, a_writer_that_never_publishes_is_still_seen, a_note_change_from_another_process_is_seen_by_the_next_version_check, a_missing_garbled_or_stale_version_file_falls_back_to_the_database. Log: scratchpad t029_test.log in the session scratchpad.
- integration: n/a: the crate's cross-process tests (child process via run_child) are the integration rung and ran inside unit.
- component: n/a: no separate component harness touched.
- e2e: n/a: no end-to-end harness touched by this diff.
- Mutation (stamp comparison disabled): NOT RUN in this audit, turn budget. The worker's claim (3 of 4 fail) is therefore cited, not re-verified.
- Timing (version_check_cost): NOT re-run (per instruction). Worker numbers cited: fast 1.4/1.6 ms, slow 23.8/25.0 ms, original 33.9/58.4 ms, on a busy machine.

## Positive + Pluto Witness
- Positive: a publishing writer in another process is seen by the next kb_version (test a_note_change_from_another_process...); unchanged calls take no slow path (assert_fast_once_settled).
- Witness (negative): a commit that skips the publish is seen (a_writer_that_dies...); a non-publishing child writer is seen (a_writer_that_never_publishes...); garbled, empty, wrong-field-count, wrong-value and an old record put back after a DB change all fall back to the database (a_missing_garbled...).
- Steady-state fast path: after any publishing handle closes, the record matches the database stat, so the next kb_version returns from the file. Readers also publish on close (StoreDb drop), so a read-only open does not invalidate the record permanently. The record is rewritten only after the redb close, so a reader cannot self-invalidate: the stamp is taken after the last write the reader causes. Under concurrent opens the fast path misses and the slow path (which takes the OS lock) runs; this costs time, not correctness, and there is no livelock because each close republishes.
- Extra cost: each DB open now also does one small temp write plus rename under the lock. Not separately measured; filter_active measured 29-30 ms after the change (the worker's run), so the added cost is small against the open itself.

## Advisory Notes
- P2 residual: a stale record can validate only if a writer that skipped the publish (a crash after commit before the guard runs, or a build that never publishes) commits with the same file length and within the same filesystem timestamp tick as the last publish. redb open is slower than one NTFS tick on this machine, so the worker's argument holds here, but it is timing-based, not a proof, and a faster disk could reach it. Suggest a note in the code or a stronger stamp (for example also comparing redb's transaction count after the slow path) as a follow-up.
- P2: Windows read of kb.version during a rename can fail with a sharing error; this falls back to the slow path (correct, slower). Not reproduced.
- P2: mutation check and timing re-run not done in this audit; cite the worker's numbers.

## Verdict
- Verdict: PASS (approve with notes). The fast path hits in steady state, a stale value cannot validate except through the same-tick timing residual above (not proven reachable on this platform), and the four named tests pass.
