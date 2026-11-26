# T023 measurements (version 0.8.0, commit 19f4b8c)

Run: `cargo test --offline --locked -p memb-rag the_hot_set -- --nocapture` (debug build, Windows).

| Measure | Result | Limit |
|---|---|---|
| Hot set, 1,001 notes x 768 dims (i8 + id + scale) | 780,780 bytes (0.78 MB), stored row = size + 16 | under 5 MB |
| In-process hit (`hot_cache_lookup`) | 1.3 to 1.75 microseconds | under 1 ms |
| `hot_set()` hit including version check (opens the database) | about 45 ms | recorded, not a limit |
| Load of the persisted hot set (nothing in memory) | 71 to 82 ms | recorded |
| Full reads of all notes during hits and during 50 expansions | 0 | 0 |
| Notes embedded after one edit | exactly 1 | unchanged notes never re-embed |
| Notes embedded for a new note with text identical to an existing one | 0 | content-hash key |
| i8 vs f32 top-10 agreement (40 queries, 1,000 clustered 768-dim vectors) | 0.995 | decides the f32 rescore departure |
