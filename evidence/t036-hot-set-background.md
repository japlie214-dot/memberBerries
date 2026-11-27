# T036: a search never waits for the vector index (release build, 2026-11-27)

Command: `cargo test --release -p memb-rag real_model_hot_set -- --ignored --nocapture`
with the installed EmbeddingGemma 2 q4 model, 256-wide vectors (`MEMB_EMBED_MRL_DIM=256`), 100 notes
(one target note about red fruit, 99 about database locks). The model was loaded once before the
timings (a hook or the server pays that elsewhere); other builds were running on the machine at the
same time, so the numbers are conservative.

| step | before (blocking build) | after |
|---|---|---|
| first search, index missing, 100 notes | about 33 s (the build ran inside the search; 13.4 s for 30 notes, 150.8 s for 470 in the T036 benchmark) | **0.51 s** (and 0.72 s in an earlier, busier run): query embedding + lexical, index builds in the background |
| background build of the 100 notes | n/a | 33.0 s (about 0.33 s per note, the same cost as before, now off the search path) |
| search after the build | 0.3 s | 0.32 s, the vector match ("bright red apple" finds "scarlet apples") is first |

Behaviour, each covered by a test in `crates/memb-rag/src/lib.rs`:

- `a_search_never_waits_for_the_vector_index_and_the_background_build_completes_it`: slow fake
  embedder (60 ms per note, 41 notes, about 2.5 s to build). The first search returns in under
  0.5 s with an empty partial set and `building: true`; a second search neither waits nor starts a
  second build; the complete set appears once the build ends; every note was embedded exactly once.
- `an_interrupted_build_resumes_without_embedding_the_finished_notes_again`: the second chunk of
  41 notes (16, 16, 9) fails; the 16 finished notes are saved; the next build embeds only the other
  25.
- `without_background_building_a_search_uses_cached_vectors_and_starts_no_build`: a hook or CLI
  search ranks with the 8 cached vectors of 21 notes, embeds nothing, starts nothing.
- The existing Snowflake and loopback-server cache tests were changed to the new contract: the first
  search answers lexically and the background build fills the index; the second search uses it.

`memb doctor` reports the index ("up to date for N active notes", "out of date or unfinished: M
document vectors saved ...", or "not built yet").
