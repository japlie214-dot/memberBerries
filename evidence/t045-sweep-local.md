# Vector score floor sweep: local EmbeddingGemma 2 q4 (30 notes, 30 on-topic questions, 12 off-topic questions, 256-wide i8 cosine as memb stores it)

Backend default floor in this build: 0.60

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
| 0.600 | 30/30 | 0.97 | 5.0 | 6/12 | 1.0 | 5.0 | 0.90 | 4.5 | 1260 |
| 0.625 | 29/30 | 0.97 | 4.9 | 0/12 | 0.0 | 5.0 | 0.90 | 4.4 | 1259 |
| 0.650 | 29/30 | 0.97 | 3.9 | 0/12 | 0.0 | 4.9 | 0.90 | 4.4 | 1243 |
| 0.675 | 29/30 | 0.97 | 2.9 | 0/12 | 0.0 | 4.8 | 0.90 | 4.4 | 1218 |
| 0.700 | 20/30 | 0.67 | 1.5 | 0/12 | 0.0 | 4.7 | 0.87 | 4.4 | 1195 |
| 0.750 | 12/30 | 0.40 | 0.5 | 0/12 | 0.0 | 4.6 | 0.70 | 4.4 | 1171 |
| default (0.60) | 30/30 | 0.97 | 5.0 | 6/12 | 1.0 | 5.0 | 0.90 | 4.5 | 1260 |
