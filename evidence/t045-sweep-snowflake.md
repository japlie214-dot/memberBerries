# Vector score floor sweep: Snowflake AI_EMBED (30 notes, 30 on-topic questions, 12 off-topic questions, 768-wide i8 cosine as memb stores it)

Backend default floor in this build: 0.25

| scores | n | min | p10 | p50 | p90 | max |
|---|---|---|---|---|---|---|
| right note for its question | 30 | 0.239 | 0.292 | 0.403 | 0.486 | 0.577 |
| best wrong note per on-topic question | 30 | 0.212 | 0.262 | 0.291 | 0.370 | 0.462 |
| all wrong notes (on-topic questions) | 870 | 0.003 | 0.107 | 0.174 | 0.249 | 0.462 |
| best note per off-topic question | 12 | 0.125 | 0.146 | 0.216 | 0.244 | 0.260 |

Lexical search alone returns 8.2 of at most 10 notes for an off-topic question on average (it has no score floor).

| floor | right note clears it | vector only: right note in top 5 | vector only: notes per on-topic question | vector only: off-topic questions with any note | vector only: notes per off-topic question | hybrid: notes per on-topic question | hybrid: recall@5 | hybrid: notes per off-topic question | hybrid: JSON bytes per on-topic hook |
|---|---|---|---|---|---|---|---|---|---|
| 0.000 | 30/30 | 1.00 | 5.0 | 12/12 | 5.0 | 5.0 | 0.93 | 5.0 | 1264 |
| 0.250 | 29/30 | 0.97 | 3.2 | 1/12 | 0.2 | 4.9 | 1.00 | 4.6 | 1248 |
| 0.420 | 11/30 | 0.37 | 0.4 | 0/12 | 0.0 | 4.7 | 0.70 | 4.4 | 1180 |
| 0.500 | 3/30 | 0.10 | 0.1 | 0/12 | 0.0 | 4.6 | 0.67 | 4.4 | 1172 |
| 0.550 | 1/30 | 0.03 | 0.0 | 0/12 | 0.0 | 4.6 | 0.67 | 4.4 | 1172 |
| 0.600 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.625 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.650 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.675 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.700 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.750 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| default (0.25) | 29/30 | 0.97 | 3.2 | 1/12 | 0.2 | 4.9 | 1.00 | 4.6 | 1248 |
