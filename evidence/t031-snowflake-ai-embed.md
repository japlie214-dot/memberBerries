# Snowflake AI_EMBED: live check (memb 0.12.0 development build, 2026-11-26)

Account in `ap-southeast-1` (Singapore), warehouse `SHARED_ID_XS`, key-pair sign-in with an unencrypted PKCS#8 key. Only the project's own synthetic corpus (30 notes, 30 paraphrased questions) was sent; no real notes.

| model | query prefix | width | first call ms | single-text p50 ms | batch of 16 ms | recall@1 | recall@3 | recall@5 | MRR@10 | 256-wide recall@1 | 256-wide recall@3 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| snowflake-arctic-embed-m-v1.5 | none | 768 | 1191 | 816 | 1464 | 0.70 | 0.87 | 0.93 | 0.80 | 0.70 | 0.87 |
| snowflake-arctic-embed-m-v1.5 | retrieval prefix | 768 | 1191 | 816 | 1464 | 0.93 | 1.00 | 1.00 | 0.97 | 0.90 | 1.00 |
| snowflake-arctic-embed-l-v2.0 | none | 1024 | 1015 | 1050 | 1607 | 0.93 | 1.00 | 1.00 | 0.96 | 0.90 | 1.00 |
| snowflake-arctic-embed-l-v2.0 | retrieval prefix | 1024 | 1015 | 1050 | 1607 | 0.60 | 0.77 | 0.87 | 0.72 | 0.57 | 0.77 |

Sign-in worked on the first request that reached SQL (the key-pair JWT is accepted), and the cross-region call from `ap-southeast-1` works.

## Through memb's own search path

Run: `cargo test --release -p memb --test embed_bench snowflake_through -- --ignored --nocapture` with the default model (snowflake-arctic-embed-m-v1.5, retrieval prefix on queries, 768-wide i8 vectors, hybrid with lexical search):

- first search (index of 30 notes built in 2 statements, plus the query): 5.3 s
- warm search (one statement for the query): p50 1.2 s, p95 3.1 s
- hybrid recall@1 0.70, @3 0.87, @5 1.00, MRR 0.80, against 0.93 / 1.00 / 1.00 / 0.97 for vectors alone: the same fusion weakness the local model showed (ticket T039)

## Choosing the default

- `snowflake-arctic-embed-m-v1.5` with the retrieval prefix: best ranking (recall@1 0.93, MRR 0.97) and the fastest (about 0.8 s for one text).
- `snowflake-arctic-embed-l-v2.0` without the prefix: nearly as good (recall@1 0.93, MRR 0.96) and about 25% slower (1.05 s); with the prefix it is much worse (recall@1 0.60), so memb never prefixes it.
- 256-wide (Matryoshka) vectors lose 3 points of recall@1 for the m model (0.90) and nothing at recall@3.
- Both beat the local 4-bit EmbeddingGemma on this corpus for ranking (its 256-wide vectors reach recall@3 0.97), but a Snowflake query takes about a second against 0.15 s locally, and it needs a network round trip and a running warehouse.

## What the live run taught the client

- AI_EMBED takes the model as a string literal: a bind variable is refused (the name is validated, then written into the statement).
- Results of a 16-text batch arrive in several partitions: the first inline, the rest fetched with `?partition=N` (gzip, so curl runs with `--compressed`). Without this a batch returned 12 of 16 vectors.
