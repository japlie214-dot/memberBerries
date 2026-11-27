# T045: the vector score floor per embedding backend (2026-11-27, memb 0.16.0 + T045)

Why: T032 chose a floor of 0.60 from a sweep on the local EmbeddingGemma 2 q4 model only, and shipped
it for every backend (0.15.0, 0.16.0). kb-qa (Case C010) flagged that the cosine range of another
backend could sit entirely below 0.60. The sweep below proves it does, for Snowflake.

Method: the same synthetic corpus for every backend (30 notes, 30 paraphrased on-topic questions with
one right note each, 12 off-topic questions), vectors made the way memb stores them, then the real
store and hybrid search. Run with
`cargo test --release -p memb --test embed_bench injection_floor_sweep -- --ignored --nocapture` (local;
`MEMB_BENCH_GEMMA_DIR`) and `injection_floor_sweep_backend` with `MEMB_BENCH_BACKEND=snowflake` (the
Snowflake variables of `wiki/Configuration.md`). The complete tables are in
`evidence/t045-sweep-local.md` and `evidence/t045-sweep-snowflake.md`. The Snowflake run sent only the
synthetic corpus (about 72 `AI_EMBED` statements) to the account in config.toml, after the user approved it.

## Score ranges (cosine of the stored i8 vectors)

| backend | width | right note: min / p50 / max | best off-topic note: min / p50 / max | best wrong on-topic note: max |
|---|---|---|---|---|
| local EmbeddingGemma 2 q4 | 256 | 0.617 / 0.741 / 0.839 | 0.543 / 0.607 / 0.622 | 0.783 |
| Snowflake arctic-embed-m-v1.5 (retrieval prefix) | 768 | **0.239 / 0.403 / 0.577** | **0.125 / 0.216 / 0.260** | 0.462 |
| loopback llama-server | n/a | not measured: no llama-server or model on this machine | | |

The two backends live on different scales: the highest Snowflake right-note score (0.577) is below the
lowest local one (0.617).

## What each floor does (vector side and hybrid search)

Local (the floor of 0.60 chosen in T032 is confirmed; the sweep was rerun here on the new bench core):

| floor | right notes kept | off-topic questions with any vector hit | hybrid recall@5 |
|---|---|---|---|
| 0.25 | 30/30 | 12/12 | 0.90 |
| 0.55 | 30/30 | 11/12 | 0.90 |
| **0.60** | **30/30** | **6/12** | **0.90** |
| 0.625 | 29/30 | 0/12 | 0.90 |
| 0.70 | 20/30 | 0/12 | 0.87 |

Snowflake:

| floor | right notes kept | off-topic questions with any vector hit | hybrid recall@5 |
|---|---|---|---|
| 0.00 | 30/30 | 12/12 | 0.93 |
| **0.25** | **29/30** | **1/12** | **1.00** |
| 0.42 | 11/30 | 0/12 | 0.70 |
| 0.50 | 3/30 | 0/12 | 0.67 |
| **0.60 (what 0.15.0 and 0.16.0 applied)** | **0/30** | 0/12 | **0.63** |

**Finding:** with a 0.60 floor every Snowflake vector hit was dropped, and a Snowflake user silently got
lexical-only search (recall@5 0.63 against 1.00 at 0.25) from 0.15.0 on. At 0.25 Snowflake keeps 29 of
30 right notes and lets 1 of 12 off-topic questions through; the right-note minimum (0.239) and the
off-topic maximum (0.260) overlap slightly, so no floor separates them perfectly and 0.25 is the best
recall.

## Decision

The default floor is resolved per backend (`memb_rag::default_score_floor`):

| backend | default floor | basis |
|---|---|---|
| local EmbeddingGemma | 0.60 | measured (T032, confirmed here) |
| Snowflake | 0.25 | measured here (29/30 right notes kept, hybrid recall@5 1.00) |
| loopback llama-server | 0.25 | **not measured**; the permissive fallback, so it can never lose all its vector hits |
| vectors handed in by a library caller | 0.25 | unknown scores, same fallback |

`[retrieval] score_floor` in config.toml is now optional: unset, nothing is exported and each search
uses the default of the backend that made its vectors; set, it applies to every backend as before.
`MEMB_RETRIEVAL_SCORE_FLOOR` still wins over the file. A loopback llama-server still needs its own
sweep (`MEMB_BENCH_BACKEND=loopback` with `MEMB_EMBED_PORT` and `MEMB_EMBED_MODEL_ID`) before it gets a
tuned value; until then it keeps 0.25.

Caveats: 30 notes and 42 questions, one run per backend (one question moves recall by 3 points); only
the default Snowflake model (arctic-embed-m-v1.5) was measured, a different `MEMB_SNOWFLAKE_MODEL` has
its own range; the corpus is synthetic English text.
