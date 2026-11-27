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
| loopback llama-server (EmbeddingGemma 300M Q8_0, added in T046) | 768 | 0.206 / 0.459 / 0.657 | 0.045 / 0.117 / 0.166 | 0.497 |
| loopback llama-server, `MEMB_EMBED_MRL_DIM=256` | 256 | 0.313 / 0.526 / 0.690 | 0.123 / 0.198 / 0.296 | 0.534 |

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
| loopback llama-server | 0.25 | measured in T046 at both widths: 768 wide keeps 29/30 right notes with 0/12 off-topic leaks and recall@5 1.00; 256 wide keeps 30/30 with 3/12 leaks and recall@5 0.97 (`evidence/t045-sweep-loopback.md`, `evidence/t045-sweep-loopback-256.md`) |
| vectors handed in by a library caller | 0.25 | unknown origin: the permissive fallback |

`[retrieval] score_floor` in config.toml is now optional: unset, nothing is exported and each search
uses the default of the backend that made its vectors; set, it applies to every backend as before.
`MEMB_RETRIEVAL_SCORE_FLOOR` still wins over the file.

## Loopback llama-server (added in T046)

The first version of this file marked the loopback backend as not measured (no server on the machine
then). A llama-server and the EmbeddingGemma 300M Q8_0 GGUF were found under the git-ignored
`dist/embedding-runtime` of an earlier phase; the server was started by hand on 127.0.0.1:18081
(`-m embeddinggemma-300M-Q8_0.gguf --embedding --pooling mean`), the sweep was run at both vector
widths (`MEMB_BENCH_BACKEND=loopback MEMB_EMBED_PORT=18081 MEMB_EMBED_MODEL_ID=...`) and the server was
stopped afterwards. 0.25 is supported at both widths and stays the default: at 768 wide it keeps 29 of
30 right notes with no off-topic leak (recall@5 1.00); at 256 wide it keeps 30 of 30 with 3 of 12
off-topic questions leaking (recall@5 0.97). A tighter floor would fit one width and not the other (about
0.20 at 768 wide and about 0.30 at 256 wide separate this sample perfectly), which one run of 30
questions does not justify. Only this model and quantization behind the port were measured: another
model behind `embeddings.port` has its own range, and `retrieval.score_floor` overrides the default.

Caveats: 30 notes and 42 questions, one run per backend (one question moves recall by 3 points); only
the default Snowflake model (arctic-embed-m-v1.5) was measured, a different `MEMB_SNOWFLAKE_MODEL` has
its own range; the corpus is synthetic English text.
