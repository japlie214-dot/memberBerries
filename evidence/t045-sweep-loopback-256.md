# Vector score floor sweep: loopback llama-server (30 notes, 30 on-topic questions, 12 off-topic questions, 256-wide i8 cosine as memb stores it)

Backend default floor in this build: 0.25

| scores | n | min | p10 | p50 | p90 | max |
|---|---|---|---|---|---|---|
| right note for its question | 30 | 0.313 | 0.369 | 0.526 | 0.610 | 0.690 |
| best wrong note per on-topic question | 30 | 0.282 | 0.318 | 0.387 | 0.511 | 0.534 |
| all wrong notes (on-topic questions) | 870 | 0.034 | 0.143 | 0.235 | 0.331 | 0.534 |
| best note per off-topic question | 12 | 0.123 | 0.163 | 0.198 | 0.274 | 0.296 |

Lexical search alone returns 8.2 of at most 10 notes for an off-topic question on average (it has no score floor).

| floor | right note clears it | vector only: right note in top 5 | vector only: notes per on-topic question | vector only: off-topic questions with any note | vector only: notes per off-topic question | hybrid: notes per on-topic question | hybrid: recall@5 | hybrid: notes per off-topic question | hybrid: JSON bytes per on-topic hook |
|---|---|---|---|---|---|---|---|---|---|
| 0.000 | 30/30 | 1.00 | 5.0 | 12/12 | 5.0 | 5.0 | 0.97 | 5.0 | 1269 |
| 0.250 | 30/30 | 1.00 | 4.9 | 3/12 | 0.2 | 5.0 | 0.97 | 4.5 | 1270 |
| 0.420 | 23/30 | 0.77 | 1.2 | 0/12 | 0.0 | 4.8 | 0.93 | 4.4 | 1211 |
| 0.500 | 17/30 | 0.57 | 0.7 | 0/12 | 0.0 | 4.8 | 0.80 | 4.4 | 1205 |
| 0.550 | 9/30 | 0.30 | 0.3 | 0/12 | 0.0 | 4.7 | 0.70 | 4.4 | 1180 |
| 0.600 | 7/30 | 0.23 | 0.2 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.625 | 3/30 | 0.10 | 0.1 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.650 | 2/30 | 0.07 | 0.1 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.675 | 1/30 | 0.03 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.700 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| 0.750 | 0/30 | 0.00 | 0.0 | 0/12 | 0.0 | 4.6 | 0.63 | 4.4 | 1172 |
| default (0.25) | 30/30 | 1.00 | 4.9 | 3/12 | 0.2 | 5.0 | 0.97 | 4.5 | 1270 |
