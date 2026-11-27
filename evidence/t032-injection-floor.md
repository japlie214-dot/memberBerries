# T032: what a hook injects, and the vector score floor (2026-11-27)

Decision: `retrieval.score_floor` defaults to **0.60** (the plan said 0.42, the code had an undisclosed 0.25),
`retrieval.max_inject_tokens` to 750 (3000 bytes) and `retrieval.top_k` to 5.

## What a hook injected before this change

The hook put every note `search` returned into `additionalContext` as JSON, with no size limit: up to 5 search
seeds plus one-hop parents and children, at most 8 notes, and a note may be 4096 bytes of description plus a title
of 160 bytes. So one prompt could carry about 34 KB (about 8,500 tokens). On the synthetic corpus below (30 short
notes) a hook injected 5 notes and 1,260 bytes of JSON on average, so the cap costs nothing there; it matters for
real notes. The cap is now enforced by `memb hook` (`crates/memb/src/main.rs`): notes are added best match first,
a note that would not fit is left out (and not marked as injected, so it can come on a later prompt), and a note
bigger than the whole budget is never injected. Tests: `a_hook_never_injects_more_than_the_configured_budget_and_skipped_notes_come_later`.

## Why 0.60 and not 0.42 or 0.25

The phase values were tuned for another model and quantization. Here (EmbeddingGemma 2 q4 ONNX, 256-wide, i8, as memb
stores it) cosines are compressed into a high band: a typical unrelated note scores about 0.6, so 0.25 and 0.42
never drop anything (every row up to 0.50 below is identical to no floor at all). The floor only starts to act
above about 0.55. At 0.60 every right note on this corpus still clears it (30/30) and half of the off-topic
questions (6 of 12) get no vector note at all; at 0.625 the lowest right note (0.617) is lost and no off-topic
question gets a vector note (the best off-topic score was 0.622); at 0.70 a third of the right notes are lost.
0.60 is the highest round value that loses nothing here; 0.625 to 0.65 fits this corpus slightly better but is
tuned to 12 questions and would lose a right note, so it was not chosen. Alternatives and how to pick them:
0.55 (barely filters), 0.625 to 0.65 (stricter, loses about 1 right note in 30, no off-topic vector hits), 0.42 or
0.25 (the same as no floor with this model).

Limits of this measurement: 30 synthetic notes, 30 on-topic and 12 off-topic questions, one model. The floor
applies to every embedding backend, and other backends have other cosine ranges (the Snowflake or llama-server
backends are not measured here): if you use one, run the sweep below and set `retrieval.score_floor`.
Lexical search has no score floor: it returned 8.2 notes of at most 10 on average for an off-topic question, and
the lexical hits fill the 5 seeds, so the hybrid columns below do not move with the floor. The floor removes
vector-only noise; it does not make memb return nothing for an unrelated prompt (a lexical floor would be a
separate change).

## Measurement

Command (release build, installed model):

`MEMB_BENCH_GEMMA_DIR=%LOCALAPPDATA%\memberBerries\models\embeddinggemma-2 MEMB_BENCH_OUT=<file> cargo test --release -p memb --test embed_bench injection_floor_sweep -- --ignored --nocapture`

The test is `injection_floor_sweep` in `crates/memb/tests/embed_bench.rs`; the corpus is that file's 30 notes and
30 paraphrased questions, plus 12 questions about nothing in it. Output:

# Vector score floor sweep (30 notes, 30 on-topic questions, 12 off-topic questions; 256-wide i8 cosine as memb stores it)

| scores | n | min | p10 | p50 | p90 | max |
|---|---|---|---|---|---|---|
| right note for its question | 30 | 0.617 | 0.681 | 0.741 | 0.817 | 0.839 |
| best wrong note per on-topic question | 30 | 0.640 | 0.659 | 0.701 | 0.724 | 0.783 |
| all wrong notes (on-topic questions) | 870 | 0.499 | 0.565 | 0.615 | 0.672 | 0.783 |
| best note per off-topic question | 12 | 0.543 | 0.557 | 0.607 | 0.613 | 0.622 |

Lexical search alone returns 8.2 of at most 10 notes for an off-topic question on average (it has no score floor).

| floor | right note clears it | vector only: right note in top 5 | vector only: notes per on-topic question | vector only: off-topic questions with any note | vector only: notes per off-topic question | hybrid: notes per on-topic question | hybrid: recall@5 | hybrid: notes per off-topic question | hybrid: JSON bytes per on-topic hook |
|---|---|---|---|---|---|---|---|---|---|
| 0.000 | 30/30 | 0.97 | 5.0 | 12/12 | 5.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.250 | 30/30 | 0.97 | 5.0 | 12/12 | 5.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.420 | 30/30 | 0.97 | 5.0 | 12/12 | 5.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.500 | 30/30 | 0.97 | 5.0 | 12/12 | 5.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.550 | 30/30 | 0.97 | 5.0 | 11/12 | 3.9 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.600 | 30/30 | 0.97 | 5.0 | 6/12 | 1.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.625 | 29/30 | 0.97 | 4.9 | 0/12 | 0.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.650 | 29/30 | 0.97 | 3.9 | 0/12 | 0.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.675 | 29/30 | 0.97 | 2.9 | 0/12 | 0.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.700 | 20/30 | 0.67 | 1.5 | 0/12 | 0.0 | 5.0 | 0.90 | 5.0 | 1260 |
| 0.750 | 12/30 | 0.40 | 0.5 | 0/12 | 0.0 | 5.0 | 0.90 | 5.0 | 1260 |
