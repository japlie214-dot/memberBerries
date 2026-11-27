# T038 embedding threads: measurements and the chosen default

Machine: 12 physical cores, 15897 MiB RAM, AVX-512 false, AVX2+FMA true; tier CpuLite. EmbeddingGemma 2 (ONNX q4), release build of the T038 working tree (memb 0.13.3 plus the T038 change), 2026-11-27.

## How it was run

`crates/memb/tests/embed_bench.rs` in quick mode (short items only), once per thread count, run directly from the built test binary:

    MEMB_BENCH_GEMMA_DIR=%LOCALAPPDATA%\memberBerries\models\embeddinggemma-2 MEMB_BENCH_QUICK=1 MEMB_EMBED_THREADS=N MEMB_BENCH_OUT=tN.md ^
      embed_bench-<hash>.exe --ignored --nocapture embeddinggemma_q4        (N = 2, 4, 6, 8, 12)

A gate waited for no rustc, link, memb or embed_bench process and total CPU at or below 35% for 9 s, up to 15 minutes per run. The gate never opened: other workers were building and testing the whole time, and three stale `find /` processes from earlier sessions (started 25 and 26 November, not ours, not touched) pin about three cores. Every run therefore started after the full 905 s wait, **not on an idle machine**, with total CPU between 28% and 85% right after each run.

## Results (p50 ms; "first embedding" loads the model)

| threads | first embedding | query, short (10 tokens) | query, 100 words | document, one line | document, paragraph | warm end-to-end search |
|---|---|---|---|---|---|---|
| 2 | 9,606 ms | 259 (contended) | 661 | 178 | 680 | 292 |
| 4 | 13,038 ms | 168 (contended) | 513 | 147 | 418 | 251 |
| 6 | 3,395 ms | 87 | 327 | 106 | 324 | 181 |
| 8 | 6,445 ms | 97 | 307 | 94 | 299 | 181 |
| 12 | 4,527 ms | 94 | 304 | 100 | 276 | 207 |

Reading the table:

- The 2- and 4-thread rows were visibly disturbed by the other builds (p95 of 358 and 204 ms, a first load of 9.6 and 13 s). The earlier quiet run in `evidence/t036-embeddinggemma-q4-benchmark.md` measured the 2-thread short query at 154 ms (and 6 threads at 129 ms, 12 at 86 ms), so treat 2 and 4 here as upper bounds, not as clean numbers.
- The 6, 8 and 12 rows have tight spreads (p95 within 10 to 25% of p50) and agree with each other: the curve is flat from 6 threads on (short query 87, 97, 94 ms; 100-word query 327, 307, 304 ms). Going from 6 to 12 threads buys at most about 10% on longer texts and nothing on short ones, while claiming twice the cores.
- The load times (first embedding) are dominated by disk and by the contention and are not used for the choice.

## The default

Half of the physical cores, at least 2, at most 8: `memb_embed::gemma::default_threads(physical_cores)` = `(cores / 2).clamp(2, 8)`, with the core count from `memb_win::probe_hardware()`. On this 12-core machine that is 6, where the measured curve has flattened. Half rather than all cores leaves room for the editor, the CLI that is calling memb, and a hook that runs next to the long-lived server. The cap of 8 follows the flat part of the curve: nothing measured above 6 to 8 threads pays. A 4-core machine gets 2 (the old default), an 8-core machine 4, 16 or more cores 8.

What the data does and does not support (stated plainly after QA review): the rows that are not contended (6, 8 and 12 threads: 87, 97 and 94 ms for a short query) show that 6 threads is not slower than 8 or 12, and the earlier quiet T036 run showed 2 threads clearly slower (154 ms against 86 ms at 12). They do **not** show that 6 beats 8 or 12: those three differ by less than the run-to-run noise. "Half the physical cores, at most 8" is a resource-based rule of thumb that the measurements are consistent with, not a measured optimum. The 2- and 4-thread rows of this sweep (259 and 168 ms) ran on a busy machine and are upper bounds only.

Override: `embeddings.threads` in config.toml (1 to 64) or `MEMB_EMBED_THREADS`; the environment wins over the file. An invalid environment value falls back to the default; an invalid config value is a startup error naming the key.

Caveat: one machine, measured under contention for 2 and 4 threads. The formula is a rule of thumb; `embeddings.threads` exists so it can be changed without a new build.
