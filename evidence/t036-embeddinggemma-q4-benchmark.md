# EmbeddingGemma 2 (ONNX q4) benchmark on this machine (release build, 2026-11-26; the main table ran on memb 0.10.0 with the default 2 threads, the thread table on 0.11.0 where `MEMB_EMBED_THREADS` exists)

12 physical cores, 15897 MiB RAM, AVX-512 false, AVX2+FMA true; tier CpuLite; model uses 2 intra-op threads

## Load and memory

- first embedding (loads the model, ONNX session, tokenizer): 5724 ms; vector width 768
- process private memory: 5 MiB before, 164 MiB with the model loaded

## Latency per text (sequential, release build)

| text | approx tokens | n | p50 ms | p95 ms | max ms |
|---|---|---|---|---|---|
| query, short | 10 | 40 | 154 | 173 | 189 |
| query, 100 words | 130 | 20 | 582 | 651 | 682 |
| query, 600 words (cut to 512 tokens) | 780 | 10 | 3008 | 3596 | 3596 |
| document, one line | 18 | 40 | 183 | 215 | 235 |
| document, paragraph | 117 | 20 | 485 | 542 | 563 |
| document, page | 520 | 10 | 2555 | 3187 | 3187 |
| document, long (1,500 words) | 1950 | 5 | 13845 | 14686 | 14686 |

- memory after the latency runs: 689 MiB

## Retrieval quality (30 notes, 30 paraphrased questions, one right note each)

| method | recall@1 | recall@3 | recall@5 | MRR@10 |
|---|---|---|---|---|
| lexical only (today's default) | 0.40 | 0.47 | 0.63 | 0.48 |
| vectors, 768-wide, f32 (reference) | 0.80 | 1.00 | 1.00 | 0.89 |
| vectors, 512-wide, f32 | 0.80 | 0.97 | 1.00 | 0.89 |
| vectors, 256-wide, f32 | 0.77 | 0.93 | 0.97 | 0.85 |
| vectors, 256-wide, i8 (what memb stores) | 0.77 | 0.97 | 0.97 | 0.85 |
| vectors, 128-wide, i8 | 0.63 | 0.87 | 0.90 | 0.74 |
| hybrid search as shipped (256-wide i8 + lexical, RRF) | 0.67 | 0.87 | 0.90 | 0.76 |

## End-to-end search through memb (30-note store)

- first search, building the hot set (30 documents embedded): 13401 ms
- warm search (query embedded, hot set in memory): p50 314 ms, p95 391 ms
- lexical search alone: p50 33.9 ms

## Scale: 500 notes

- hot set rebuild after adding 470 notes (all new, so 470 embeddings, about 30 words each): 150.8 s, 321 ms per document
- hot set size: 130 KiB (268 bytes per note at 256 dims)
- warm search at 500 notes: p50 330 ms, p95 393 ms
- process private memory now: 854 MiB
- trim (memory governor / idle unload): dropped 1 model(s) in 673 ms; memory after: 691 MiB

## Effect of the thread count (quick run: short items only, same machine)

`MEMB_EMBED_THREADS` (new, default 2) sets the ONNX Runtime threads inside one embedding.

| threads | first embedding (load) | query, short p50 | document, one line p50 | document, paragraph p50 | warm end-to-end search p50 |
|---|---|---|---|---|---|
| 2 (default) | 5,724 ms | 154 ms | 183 ms | 485 ms | 314 ms |
| 6 | 3,912 ms | 129 ms | 108 ms | 287 ms | 221 ms |
| 12 | 3,081 ms | 86 ms | 86 ms | 239 ms | 208 ms |

## Reading the numbers

- **Quality is good.** On 30 paraphrased questions the 256-wide i8 vectors that memb stores find the right note in the top 3 for 97% of questions (lexical search alone: 47%). Going from 768 to 256 dimensions costs about 3 points of recall@1; i8 storage costs nothing measurable. The effect of the 4-bit quantization itself is not isolated: there is no full-precision model installed to compare against.
- **Hybrid fusion is worse than vectors alone on this corpus** (recall@1 0.67 against 0.77; MRR 0.76 against 0.85): lexical hits on common words pull wrong notes into the top 5 with equal weight.
- **Speed is the weak point.** A 10-token query takes 154 ms with the default 2 threads (86 ms with 12), a one-line document 183 ms, and cost grows roughly linearly with length (about 5 ms per token at 2 threads): a 520-token page takes 2.5 s, a 1,950-token text 14 s. Phase 2's target was 35 to 90 ms.
- **A warm search takes 210 to 330 ms**, of which only 86 to 154 ms is the model; the rest is the database opened several times per search (version check about 45 ms, lexical scan about 34 ms, expansion) which is T029.
- **Building the hot set blocks the search.** 30 notes took 13.4 s on the first search; 470 new notes took 150.8 s (321 ms per note). The search that triggers it waits.
- **Memory:** 164 MiB with the model loaded; it grew to 689 MiB after long inputs and 854 MiB after 500 notes; after `trim_caches` (the governor's unload) 691 MiB were still held, so unloading does not return all of it.
- **A short-lived process pays the load every time.** Measured with the release binary running a real `memb hook prompt-submit --cli cc` on an empty store with a 150-character prompt (retrieval on, 256-wide vectors, 2 threads): 369 and 489 ms with the model disabled (lexical), 6,326, 3,272 and 3,822 ms with the model installed. That is fine for the long-running MCP server, but a hook is a new process per prompt (T040, T042).
